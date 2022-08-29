#![no_std]
#![no_main]
#![feature(naked_functions, asm_sym, asm_const)]
#![feature(default_alloc_error_handler)]
#![deny(warnings)]

#[macro_use]
extern crate output;

#[macro_use]
extern crate alloc;

use self::page_table::KernelSpaceBuilder;
use crate::{mm::global, page_table::calculate_page_count};
use ::page_table::{PageTable, PageTableShuttle, Sv39, VAddr, VmMeta, VPN};
use impls::Console;
use output::log;
use riscv::register::satp;
use sbi_rt::*;

// 应用程序内联进来。
core::arch::global_asm!(include_str!(env!("APP_ASM")));

/// Supervisor 汇编入口。
///
/// 设置栈并跳转到 Rust。
#[naked]
#[no_mangle]
#[link_section = ".text.entry"]
unsafe extern "C" fn _start() -> ! {
    const STACK_SIZE: usize = 4 * 4096;

    #[link_section = ".bss.uninit"]
    static mut STACK: [u8; STACK_SIZE] = [0u8; STACK_SIZE];

    core::arch::asm!(
        "   la  sp, {stack}
            li  t0, {stack_size}
            add sp, sp, t0
            j   {main}
        ",
        stack_size = const STACK_SIZE,
        stack      =   sym STACK,
        main       =   sym rust_main,
        options(noreturn),
    )
}

extern "C" fn rust_main() -> ! {
    // bss 段清零
    extern "C" {
        static mut sbss: u64;
        static mut ebss: u64;
    }
    unsafe { r0::zero_bss(&mut sbss, &mut ebss) };
    // 初始化 `output`
    output::init_console(&Console);
    output::set_log_level(option_env!("LOG"));
    utils::test_log();
    // 打印段位置
    extern "C" {
        fn __text();
        fn __transit();
        fn __rodata();
        fn __data();
        fn __end();
    }
    log::info!("__text ----> {:#10x}", __text as usize);
    log::info!("__transit -> {:#10x}", __transit as usize);
    log::info!("__rodata --> {:#10x}", __rodata as usize);
    log::info!("__data ----> {:#10x}", __data as usize);
    log::info!("__end -----> {:#10x}", __end as usize);
    println!();
    mm::init();

    // 内核地址空间
    {
        let kernel_root = mm::Page::ZERO;
        let kernel_root = VAddr::<Sv39>::new(kernel_root.addr());
        let table = unsafe {
            PageTable::<Sv39>::from_raw_parts(
                kernel_root.val() as *mut _,
                VPN::ZERO,
                Sv39::MAX_LEVEL,
            )
        };
        let mut shuttle = PageTableShuttle {
            table,
            f: |ppn| VPN::new(ppn.val()),
        };
        shuttle.walk_mut(KernelSpaceBuilder(unsafe { global() }));
        // println!("{shuttle:?}");
        unsafe { satp::set(satp::Mode::Sv39, 0, kernel_root.floor().val()) };
    }
    // 测试内核堆分配
    {
        let mut vec = vec![0; 256];
        for (i, val) in vec.iter_mut().enumerate() {
            *val = i;
        }
        println!("{vec:?}");
        println!();
    }

    {
        use xmas_elf::{
            header::{self, HeaderPt2, Machine},
            ElfFile,
        };
        // 加载应用程序
        extern "C" {
            static apps: utils::AppMeta;
        }
        let mut count_apps = 0usize;
        let mut count_pages = 0usize;
        for (i, elf) in unsafe { apps.iter_elf() }.enumerate() {
            println!(
                "detect app[{i}] at {:?} (size: {} bytes)",
                elf.as_ptr(),
                elf.len()
            );
            let elf = ElfFile::new(elf).unwrap();
            if let HeaderPt2::Header64(pt2) = elf.header.pt2 {
                if pt2.type_.as_type() != header::Type::Executable
                    || pt2.machine.as_machine() != Machine::RISC_V
                {
                    continue;
                }
                let n = calculate_page_count(&elf);
                println!("this app needs {n} pages to load");
                println!();
                count_apps += 1;
                count_pages += n;
            }
        }
        println!("all {count_apps} apps need {count_pages} pages to load");
        println!();
    }

    system_reset(RESET_TYPE_SHUTDOWN, RESET_REASON_NO_REASON);
    unreachable!()
}

/// Rust 异常处理函数，以异常方式关机。
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("{info}");
    system_reset(RESET_TYPE_SHUTDOWN, RESET_REASON_SYSTEM_FAILURE);
    unreachable!()
}

/// 各种接口库的实现
mod impls {
    pub struct Console;

    impl output::Console for Console {
        #[inline]
        fn put_char(&self, c: u8) {
            #[allow(deprecated)]
            sbi_rt::legacy::console_putchar(c as _);
        }
    }
}

mod mm {
    use alloc::alloc::handle_alloc_error;
    use buddy_allocator::{BuddyAllocator, LinkedListBuddy, UsizeBuddy};
    use core::{
        alloc::{GlobalAlloc, Layout},
        cell::RefCell,
        ptr::NonNull,
    };

    /// 初始化全局分配器和内核堆分配器。
    pub fn init() {
        unsafe {
            let ptr = NonNull::new(MEMORY.as_mut_ptr()).unwrap();
            let len = core::mem::size_of_val(&MEMORY);
            println!(
                "MEMORY = {:#x}..{:#x}",
                ptr.as_ptr() as usize,
                ptr.as_ptr() as usize + len
            );
            GLOBAL.init(12, ptr);
            GLOBAL.transfer(ptr, len);
            ALLOC.0.borrow_mut().init(3, ptr);
        }
    }

    /// 获取全局分配器。
    #[inline]
    pub unsafe fn global() -> &'static mut MutAllocator<5> {
        &mut GLOBAL
    }

    #[repr(C, align(4096))]
    pub struct Page([u8; 4096]);

    impl Page {
        pub const ZERO: Self = Self([0; 4096]);
        pub const LAYOUT: Layout = Layout::new::<Self>();

        #[inline]
        pub fn addr(&self) -> usize {
            self as *const _ as _
        }
    }

    /// 托管空间 4 MiB
    static mut MEMORY: [Page; 1024] = [Page::ZERO; 1024];
    static mut GLOBAL: MutAllocator<5> = MutAllocator::<5>::new();
    #[global_allocator]
    static ALLOC: SharedAllocator<22> = SharedAllocator(RefCell::new(MutAllocator::new()));

    pub type MutAllocator<const N: usize> = BuddyAllocator<N, UsizeBuddy, LinkedListBuddy>;

    struct SharedAllocator<const N: usize>(RefCell<MutAllocator<N>>);
    unsafe impl<const N: usize> Sync for SharedAllocator<N> {}
    unsafe impl<const N: usize> GlobalAlloc for SharedAllocator<N> {
        #[inline]
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let mut inner = self.0.borrow_mut();
            loop {
                if let Ok((ptr, _)) = inner.allocate::<u8>(layout) {
                    return ptr.as_ptr();
                } else if let Ok((ptr, size)) = GLOBAL.allocate::<u8>(layout) {
                    inner.transfer(ptr, size);
                } else {
                    handle_alloc_error(layout)
                }
            }
        }

        #[inline]
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            self.0
                .borrow_mut()
                .deallocate(NonNull::new(ptr).unwrap(), layout.size())
        }
    }
}

mod page_table {
    use crate::mm::{MutAllocator, Page};
    use core::cmp::max;
    use page_table::{Decorator, Pos, Pte, Sv39, Update, VAddr, VmFlags, PPN};
    use xmas_elf::{program, ElfFile};

    pub struct KernelSpaceBuilder<'a, const N: usize>(pub &'a mut MutAllocator<N>);

    impl<'a, const N: usize> Decorator<Sv39> for KernelSpaceBuilder<'a, N> {
        #[inline]
        fn start(&mut self, _: Pos<Sv39>) -> Pos<Sv39> {
            Pos::new(VAddr::new(__text as usize).floor(), 0)
        }

        #[inline]
        fn arrive(&mut self, pte: &mut Pte<Sv39>, target_hint: Pos<Sv39>) -> Pos<Sv39> {
            let addr = target_hint.vpn.base().val();
            let bits = if addr < __transit as usize {
                0b1011 // X_RV <- .text
            } else if addr < __rodata as usize {
                0b1111 // XWRV <- .trampline
            } else if addr < __data as usize {
                0b0011 // __RV <- .rodata
            } else if addr < __end as usize {
                0b0111 // _WRV <- .data + .bss
            } else {
                return Pos::stop(); // end of kernel sections
            };
            *pte = unsafe { VmFlags::from_raw(bits) }.build_pte(PPN::new(target_hint.vpn.val()));
            target_hint.next()
        }

        #[inline]
        fn meet(
            &mut self,
            _level: usize,
            _pte: Pte<Sv39>,
            _target_hint: Pos<Sv39>,
        ) -> Update<Sv39> {
            let (ptr, size) = self.0.allocate::<Page>(Page::LAYOUT).unwrap();
            assert_eq!(size, Page::LAYOUT.size());
            let vpn = VAddr::new(ptr.as_ptr() as _).floor();
            let ppn = PPN::new(vpn.val());
            Update::Pte(unsafe { VmFlags::from_raw(1) }.build_pte(ppn), vpn)
        }
    }

    /// 计算应用程序总共需要多少个页。
    ///
    /// 包括存储各个加载段数据的页和页表页，以及位于低 256 GiB 最后两个 4 KiB 页的用户栈和它们的页表页。
    pub fn calculate_page_count(elf: &ElfFile) -> usize {
        // 需要的总页计数
        const COUNT_512G: usize = 1; // 2 级页表
        let mut count_1g = 0usize; // 1 级页表数量
        let mut count_2m = 0usize; // 0 级页表数量
        let mut count_4k = 0usize; // 0 级页数量

        // NOTICE ELF 文件中程序段是按虚存位置排序的，且段间不重叠，因此可以用一个指针表示已覆盖的范围
        let mut end_1g = 0usize; // 2 级页表覆盖范围
        let mut end_2m = 0usize; // 1 级页表覆盖范围

        for program in elf.program_iter() {
            if let Ok(program::Type::Load) = program.get_type() {
                let off_file = program.offset();
                let end_file = off_file + program.file_size();
                let off_mem = program.virtual_addr();
                let end_mem = off_mem + program.mem_size();
                println!("LOAD {off_file:#08x}..{end_file:#08x} -> {off_mem:#08x}..{end_mem:#08x} with {:?}", program.flags());

                // 更新 0 级页数量
                {
                    let off_mem = off_mem as usize >> 12;
                    let end_mem = (end_mem as usize + mask(12)) >> 12;
                    count_4k += end_mem - off_mem;
                }
                // 更新 0 级页表覆盖范围
                {
                    let mask_2m = mask(12 + 9);
                    end_2m = max(end_2m, off_mem as usize & !mask_2m);
                    let end_program = (end_mem as usize + mask_2m) & !mask_2m;
                    while end_2m < end_program {
                        count_2m += 1;
                        end_2m += mask_2m + 1;
                    }
                }
                // 更新 1 级页表覆盖范围
                {
                    let mask_1g = mask(12 + 9 + 9);
                    end_1g = max(end_1g, off_mem as usize & !mask_1g);
                    let end_program = (end_mem as usize + mask_1g) & !mask_1g;
                    while end_1g < end_program {
                        count_1g += 1;
                        end_1g += mask_1g + 1;
                    }
                }
            }
        }
        // 补充栈空间
        count_4k += 2;
        count_2m += 1;
        count_1g += 1;
        count_4k + count_2m + count_1g + COUNT_512G
    }

    #[inline]
    const fn mask(bits: usize) -> usize {
        (1 << bits) - 1
    }

    extern "C" {
        fn __text();
        fn __transit();
        fn __rodata();
        fn __data();
        fn __end();
    }
}
