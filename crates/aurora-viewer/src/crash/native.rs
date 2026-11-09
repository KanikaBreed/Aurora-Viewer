//! Small platform boundary: wait on the actual process, not a reused PID.
//! The native exception hook only writes to a pre-opened file; it does not
//! start processes, take application locks, initialize symbols or use the GPU.

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::io::Write;
    use std::path::Path;
    use std::sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    };
    #[cfg(target_arch = "x86_64")]
    use windows_sys::Win32::System::{
        Diagnostics::Debug::{
            ADDRESS64, AddrModeFlat, CONTEXT, CONTEXT_FULL_AMD64, GetThreadContext, PFUNCTION_TABLE_ACCESS_ROUTINE64,
            PGET_MODULE_BASE_ROUTINE64, PREAD_PROCESS_MEMORY_ROUTINE64, PTRANSLATE_ADDRESS_ROUTINE64, ReadProcessMemory,
            RtlLookupFunctionEntry, RtlVirtualUnwind, STACKFRAME64, SYMOPT_DEFERRED_LOADS, SYMOPT_DISABLE_SYMSRV_AUTODETECT,
            SYMOPT_FAIL_CRITICAL_ERRORS, SYMOPT_IGNORE_CVREC, SYMOPT_IGNORE_NT_SYMPATH, SYMOPT_NO_PROMPTS, UNW_FLAG_NHANDLER,
        },
        Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery},
        SystemInformation::IMAGE_FILE_MACHINE_AMD64,
    };
    use windows_sys::Win32::{
        Foundation::{CloseHandle, FILETIME, HANDLE, HWND, LPARAM, LRESULT, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM},
        System::{
            Diagnostics::Debug::{
                AddVectoredExceptionHandler, EXCEPTION_CONTINUE_SEARCH, EXCEPTION_EXECUTE_HANDLER, EXCEPTION_POINTERS,
                IMAGE_DEBUG_DIRECTORY, IMAGE_DEBUG_TYPE_CODEVIEW, IMAGE_DIRECTORY_ENTRY_DEBUG, IMAGE_NT_HEADERS64,
                IMAGE_NT_OPTIONAL_HDR64_MAGIC, RaiseException, RtlCaptureStackBackTrace, RtlPcToFileHeader, SetUnhandledExceptionFilter,
            },
            LibraryLoader::GetModuleHandleW,
            Threading::{
                GetCurrentProcess, GetCurrentThreadId, GetExitCodeProcess, GetProcessTimes, INFINITE, OpenProcess, OpenThread,
                PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, ResumeThread, SuspendThread, THREAD_GET_CONTEXT,
                THREAD_QUERY_INFORMATION, THREAD_SUSPEND_RESUME, WaitForSingleObject,
            },
        },
        UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DispatchMessageW, IsHungAppWindow, MSG, MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW,
            QS_ALLINPUT, RegisterClassW, TranslateMessage, WM_ENDSESSION, WM_QUERYENDSESSION, WNDCLASSW, WS_OVERLAPPED,
        },
    };

    static TRACE: OnceLock<std::fs::File> = OnceLock::new();
    static WRITING: AtomicBool = AtomicBool::new(false);
    static OOM: OnceLock<std::fs::File> = OnceLock::new();
    static OOM_WRITING: AtomicBool = AtomicBool::new(false);
    const STATUS_STACK_OVERFLOW: i32 = 0xC000_00FDu32 as i32;
    const FRAMES: usize = 64;

    struct Process(HANDLE);
    impl Drop for Process {
        fn drop(&mut self) {
            // SAFETY: this wrapper exclusively owns the handle from OpenProcess.
            unsafe { CloseHandle(self.0) };
        }
    }

    fn open(pid: u32) -> Option<Process> {
        // SAFETY: no borrowed pointers; access is limited to waiting / metadata.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        (!handle.is_null()).then_some(Process(handle))
    }

    fn started(handle: HANDLE) -> u64 {
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        // SAFETY: handle is live; all output pointers refer to local FILETIMEs.
        if unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
            return 0;
        }
        (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime)
    }

    pub fn current_process_started() -> u64 {
        // SAFETY: the pseudo-handle needs no close and is valid in this process.
        started(unsafe { GetCurrentProcess() })
    }

    pub fn is_alive(pid: u32, creation: u64) -> bool {
        let Some(process) = open(pid) else { return false };
        // SAFETY: Process owns a valid handle; this is a nonblocking query.
        started(process.0) == creation && unsafe { WaitForSingleObject(process.0, 0) } != WAIT_OBJECT_0
    }

    pub fn identifies(_creation: u64) -> bool {
        true
    }

    pub fn watch(dir: &Path, record: &super::super::Record) -> anyhow::Result<()> {
        let process = open(record.pid).ok_or_else(|| anyhow::anyhow!("cannot open viewer process"))?;
        if started(process.0) != record.process_started {
            return super::super::show_once(dir);
        }
        std::fs::write(dir.join("watching"), [])?;
        anyhow::ensure!(wait_for_exit(dir, &process, record.pid), "cannot wait for viewer");
        let mut code = 1;
        // SAFETY: valid process handle and output pointer.
        anyhow::ensure!(unsafe { GetExitCodeProcess(process.0, &mut code) } != 0, "cannot query viewer exit");
        if super::super::should_report(dir, code) {
            super::super::preserve(dir, code)?;
            super::super::show_once(dir)?;
        } else {
            // The viewer has exited and closed its files. This directory is
            // the validated UUID session supplied by our own parent only.
            let _ = std::fs::remove_dir_all(dir);
        }
        Ok(())
    }

    static ENDING: OnceLock<std::path::PathBuf> = OnceLock::new();

    /// Waits for the viewer while a hidden window hears Windows end the
    /// session: the viewer is then stopped by the system, not crashed, and
    /// the `system-ending` marker keeps that from being reported.
    fn wait_for_exit(dir: &Path, process: &Process, pid: u32) -> bool {
        let _ = ENDING.set(dir.to_owned());
        let class: Vec<u16> = "AuroraCrashWatch\0".encode_utf16().collect();
        // SAFETY: the class name lives until the end of this function, the
        // window procedure only touches static storage, and every message
        // structure is a local the calls fill.
        unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            let wc = WNDCLASSW {
                lpfnWndProc: Some(session_end),
                hInstance: instance,
                lpszClassName: class.as_ptr(),
                ..std::mem::zeroed()
            };
            RegisterClassW(&wc);
            // A hidden top-level window (not message-only): only those
            // receive the end-of-session broadcast.
            let window = CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null(),
            );
            if window.is_null() {
                return WaitForSingleObject(process.0, INFINITE) == WAIT_OBJECT_0;
            }
            // once a second, the viewer's window seen from outside: a
            // freeze that stopped the viewer's own watchdog too is still
            // recorded (#3099, #3102, #3113)
            let mut view = super::super::watchdog::OutsideView::default();
            let mut recorded = false;
            let mut stacks = super::super::watchdog::OutsideStacks::default();
            loop {
                match MsgWaitForMultipleObjects(1, &process.0, 0, 1000, QS_ALLINPUT) {
                    WAIT_OBJECT_0 => return true,
                    WAIT_TIMEOUT => {
                        let window = super::super::read_limited(&dir.join(super::super::WINDOW_FILE), 32)
                            .ok()
                            .and_then(|text| text.trim().parse::<usize>().ok());
                        if let Some(window) = window {
                            let now = std::time::Instant::now();
                            let seconds = view.look(window_hung(window), now);
                            super::super::watchdog::record_outside(dir, seconds, &mut recorded);
                            stacks.look(dir, pid, window, seconds, now);
                        }
                    }
                    r if r == WAIT_OBJECT_0 + 1 => {
                        let mut msg: MSG = std::mem::zeroed();
                        while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                            TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                    }
                    _ => return false,
                }
            }
        }
    }

    unsafe extern "system" fn session_end(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        match message {
            WM_QUERYENDSESSION => 1,
            // Only a confirmed end: another program may cancel the query.
            WM_ENDSESSION => {
                if wparam != 0
                    && let Some(dir) = ENDING.get()
                {
                    let _ = std::fs::write(dir.join("system-ending"), []);
                }
                0
            }
            // SAFETY: the arguments are the ones Windows gave this procedure.
            _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
        }
    }

    /// The stack of another thread of this process, as module + offsets,
    /// for a window that stopped answering. The thread is suspended only
    /// while its registers are read and its frames unwound into a fixed
    /// array: no allocation, lock or log meanwhile (it may hold the heap's
    /// or the logger's). Every frame is checked to stay inside the stack
    /// the thread had when suspended, so a damaged stack ends the walk
    /// instead of faulting here.
    #[cfg(target_arch = "x86_64")]
    pub fn write_thread_stack(out: &mut impl Write, thread: HANDLE) -> bool {
        #[repr(C, align(16))]
        struct Aligned(CONTEXT);
        let mut addresses = [std::ptr::null_mut::<c_void>(); FRAMES];
        let mut count = 0;
        // SAFETY: the thread is a live thread of this process whose handle
        // has the suspend and context rights; it is resumed on every path
        // below. While it is suspended, memory is only read inside the
        // committed stack region found by VirtualQuery, and the unwind
        // calls read the loaded images' unwind tables.
        unsafe {
            if SuspendThread(thread) == u32::MAX {
                return false;
            }
            let mut context = Aligned(std::mem::zeroed());
            context.0.ContextFlags = CONTEXT_FULL_AMD64;
            if GetThreadContext(thread, &mut context.0) != 0 {
                let context = &mut context.0;
                let mut region: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
                let top = if VirtualQuery(context.Rsp as *const c_void, &mut region, size_of::<MEMORY_BASIC_INFORMATION>()) != 0 {
                    region.BaseAddress as u64 + region.RegionSize as u64
                } else {
                    0
                };
                while count < FRAMES && context.Rip != 0 && context.Rsp >= region.BaseAddress as u64 && context.Rsp + 8 <= top {
                    addresses[count] = context.Rip as *mut c_void;
                    count += 1;
                    let (rsp, mut base) = (context.Rsp, 0u64);
                    let entry = RtlLookupFunctionEntry(context.Rip, &mut base, std::ptr::null_mut());
                    if entry.is_null() {
                        // A leaf function: the return address is on top.
                        context.Rip = std::ptr::read(context.Rsp as *const u64);
                        context.Rsp += 8;
                    } else {
                        let (mut data, mut frame) = (std::ptr::null_mut(), 0u64);
                        RtlVirtualUnwind(
                            UNW_FLAG_NHANDLER,
                            base,
                            context.Rip,
                            entry,
                            context,
                            &mut data,
                            &mut frame,
                            std::ptr::null_mut(),
                        );
                    }
                    if context.Rsp <= rsp {
                        break;
                    }
                }
            }
            ResumeThread(thread);
        }
        if count == 0 {
            return false;
        }
        write_modules(out, &addresses[..count], None);
        true
    }

    /// This process's threads with their names (`aurora-render`…), for
    /// the freeze report; a thread without a name is skipped.
    pub fn named_threads() -> Vec<(u32, String)> {
        // SAFETY: no argument.
        named_threads_of(unsafe { windows_sys::Win32::System::Threading::GetCurrentProcessId() })
    }

    /// The same for the process `pid` (the watcher's view of the viewer).
    fn named_threads_of(pid: u32) -> Vec<(u32, String)> {
        use windows_sys::Win32::Foundation::{INVALID_HANDLE_VALUE, LocalFree};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
        };
        use windows_sys::Win32::System::Threading::{GetThreadDescription, THREAD_QUERY_LIMITED_INFORMATION};
        let mut out = Vec::new();
        // SAFETY: every handle opened here is closed before returning, the
        // entry is sized as the snapshot calls expect, and the description
        // the system allocates is read to its zero then freed with LocalFree.
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snapshot == INVALID_HANDLE_VALUE || snapshot.is_null() {
                return out;
            }
            let mut entry: THREADENTRY32 = std::mem::zeroed();
            entry.dwSize = size_of::<THREADENTRY32>() as u32;
            let mut more = Thread32First(snapshot, &mut entry) != 0;
            while more {
                if entry.th32OwnerProcessID == pid {
                    let handle = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, entry.th32ThreadID);
                    if !handle.is_null() {
                        let mut text = std::ptr::null_mut();
                        if GetThreadDescription(handle, &mut text) >= 0 && !text.is_null() {
                            let len = (0..).take_while(|&i| *text.add(i) != 0).count();
                            let name = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
                            LocalFree(text.cast());
                            if !name.is_empty() {
                                out.push((entry.th32ThreadID, name));
                            }
                        }
                        CloseHandle(handle);
                    }
                }
                more = Thread32Next(snapshot, &mut entry) != 0;
            }
            CloseHandle(snapshot);
        }
        out
    }

    /// From the watcher, while the viewer's window is hung and its own
    /// watchdog stopped with it: the stacks of the thread that owns the
    /// window (the main one) and of the threads named in `names`, in the
    /// format of `freeze.txt`. Nothing the viewer holds can block this
    /// process: each thread is suspended while its frames are walked with
    /// the debug-help library from the viewer's own unwind tables, then
    /// resumed. The library is loaded from System32 only now, never with
    /// the viewer, and finds no symbol files: only module + offset are
    /// written, as in the viewer.
    #[cfg(target_arch = "x86_64")]
    pub fn write_outside_stacks(out: &mut Vec<u8>, pid: u32, window: usize, names: &[&str]) -> bool {
        use windows_sys::Win32::System::Threading::{PROCESS_QUERY_INFORMATION, PROCESS_VM_READ};
        use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
        let Some(help) = DebugHelp::load() else { return false };
        // SAFETY: no borrowed pointers; the rights the debug-help library
        // needs to read the modules and stacks.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid) };
        if handle.is_null() {
            return false;
        }
        let process = Process(handle);
        let mut owner = 0;
        // SAFETY: a stale handle only answers 0.
        let main = unsafe { GetWindowThreadProcessId(window as HWND, &mut owner) };
        let mut threads: Vec<(u32, String)> = Vec::new();
        if main != 0 && owner == pid {
            threads.push((main, "main".to_owned()));
        }
        threads.extend(named_threads_of(pid).into_iter().filter(|(_, name)| names.contains(&name.as_str())));
        // A folder without symbol files: no PDB search, no symbol server.
        let nowhere: Vec<u16> = "\\nowhere\0".encode_utf16().collect();
        // SAFETY: the options are flags; the path lives until the end of
        // this function and the process handle is live.
        unsafe {
            (help.set_options)(
                SYMOPT_DEFERRED_LOADS
                    | SYMOPT_FAIL_CRITICAL_ERRORS
                    | SYMOPT_NO_PROMPTS
                    | SYMOPT_IGNORE_CVREC
                    | SYMOPT_IGNORE_NT_SYMPATH
                    | SYMOPT_DISABLE_SYMSRV_AUTODETECT,
            );
            if (help.initialize)(process.0, nowhere.as_ptr(), 1) == 0 {
                return false;
            }
        }
        let mut any = false;
        for (id, name) in threads {
            out.extend_from_slice(format!("Thread: {name}\n").as_bytes());
            any |= Thread::open(id).is_some_and(|thread| help.write_stack(out, process.0, thread.0));
        }
        // SAFETY: initialized above for this handle.
        unsafe { (help.cleanup)(process.0) };
        any
    }

    // The signatures of the debug-help exports the watcher uses.
    #[cfg(target_arch = "x86_64")]
    type Export = unsafe extern "system" fn() -> isize;
    #[cfg(target_arch = "x86_64")]
    type SymSetOptions = unsafe extern "system" fn(u32) -> u32;
    #[cfg(target_arch = "x86_64")]
    type SymInitializeW = unsafe extern "system" fn(HANDLE, *const u16, i32) -> i32;
    #[cfg(target_arch = "x86_64")]
    type SymCleanup = unsafe extern "system" fn(HANDLE) -> i32;
    #[cfg(target_arch = "x86_64")]
    type StackWalk64 = unsafe extern "system" fn(
        u32,
        HANDLE,
        HANDLE,
        *mut STACKFRAME64,
        *mut c_void,
        PREAD_PROCESS_MEMORY_ROUTINE64,
        PFUNCTION_TABLE_ACCESS_ROUTINE64,
        PGET_MODULE_BASE_ROUTINE64,
        PTRANSLATE_ADDRESS_ROUTINE64,
    ) -> i32;
    #[cfg(target_arch = "x86_64")]
    type SymFunctionTableAccess64 = unsafe extern "system" fn(HANDLE, u64) -> *mut c_void;
    #[cfg(target_arch = "x86_64")]
    type SymGetModuleBase64 = unsafe extern "system" fn(HANDLE, u64) -> u64;

    /// The debug-help functions the watcher walks the viewer's stacks with.
    #[cfg(target_arch = "x86_64")]
    struct DebugHelp {
        set_options: SymSetOptions,
        initialize: SymInitializeW,
        cleanup: SymCleanup,
        stack_walk: StackWalk64,
        function_table: SymFunctionTableAccess64,
        module_base: SymGetModuleBase64,
    }

    #[cfg(target_arch = "x86_64")]
    impl DebugHelp {
        /// From System32 alone: a `dbghelp.dll` beside the viewer is never
        /// loaded. Kept loaded until the watcher ends.
        fn load() -> Option<DebugHelp> {
            use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
            let file: Vec<u16> = "dbghelp.dll\0".encode_utf16().collect();
            // SAFETY: a zero-ended name; the library stays loaded (never
            // freed), so the functions taken from it stay valid.
            unsafe {
                let library = LoadLibraryExW(file.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
                if library.is_null() {
                    return None;
                }
                let function = |name: &[u8]| GetProcAddress(library, name.as_ptr());
                // SAFETY (each transmute): the documented signature of that
                // export of dbghelp.dll, with the same ABI.
                Some(DebugHelp {
                    set_options: std::mem::transmute::<Export, SymSetOptions>(function(b"SymSetOptions\0")?),
                    initialize: std::mem::transmute::<Export, SymInitializeW>(function(b"SymInitializeW\0")?),
                    cleanup: std::mem::transmute::<Export, SymCleanup>(function(b"SymCleanup\0")?),
                    stack_walk: std::mem::transmute::<Export, StackWalk64>(function(b"StackWalk64\0")?),
                    function_table: std::mem::transmute::<Export, SymFunctionTableAccess64>(function(b"SymFunctionTableAccess64\0")?),
                    module_base: std::mem::transmute::<Export, SymGetModuleBase64>(function(b"SymGetModuleBase64\0")?),
                })
            }
        }

        /// One thread of the viewer, as `write_modules` writes it.
        fn write_stack(&self, out: &mut Vec<u8>, process: HANDLE, thread: HANDLE) -> bool {
            #[repr(C, align(16))]
            struct Aligned(CONTEXT);
            let mut addresses = Vec::with_capacity(FRAMES);
            // SAFETY: both handles are live with the rights asked; the
            // thread is resumed on every path. The walk reads the other
            // process through the system, never its memory directly.
            unsafe {
                // The routines the walk calls back: the library's own.
                let function_table: PFUNCTION_TABLE_ACCESS_ROUTINE64 = Some(self.function_table);
                let module_base: PGET_MODULE_BASE_ROUTINE64 = Some(self.module_base);
                if SuspendThread(thread) == u32::MAX {
                    return false;
                }
                let mut context = Aligned(std::mem::zeroed());
                context.0.ContextFlags = CONTEXT_FULL_AMD64;
                if GetThreadContext(thread, &mut context.0) != 0 {
                    let flat = |offset| ADDRESS64 {
                        Offset: offset,
                        Segment: 0,
                        Mode: AddrModeFlat,
                    };
                    let mut frame: STACKFRAME64 = std::mem::zeroed();
                    frame.AddrPC = flat(context.0.Rip);
                    frame.AddrFrame = flat(context.0.Rbp);
                    frame.AddrStack = flat(context.0.Rsp);
                    while addresses.len() < FRAMES
                        && (self.stack_walk)(
                            u32::from(IMAGE_FILE_MACHINE_AMD64),
                            process,
                            thread,
                            &mut frame,
                            (&mut context.0 as *mut CONTEXT).cast(),
                            None,
                            function_table,
                            module_base,
                            None,
                        ) != 0
                        && frame.AddrPC.Offset != 0
                    {
                        addresses.push(frame.AddrPC.Offset as usize);
                    }
                }
                ResumeThread(thread);
            }
            if addresses.is_empty() {
                return false;
            }
            // SAFETY: the library is initialized for this process meanwhile.
            let locate = |address: usize| unsafe { (self.module_base)(process, address as u64) } as usize;
            write_frames(out, addresses, None, locate, |base| Module::read_from(process, base));
            true
        }
    }

    /// A handle to a thread with the rights the freeze report needs.
    pub struct Thread(HANDLE);
    impl Drop for Thread {
        fn drop(&mut self) {
            // SAFETY: this wrapper exclusively owns the handle.
            unsafe { CloseHandle(self.0) };
        }
    }
    // SAFETY: a thread handle is a kernel object reference, usable from any thread.
    unsafe impl Send for Thread {}

    impl Thread {
        pub fn current() -> Option<Thread> {
            // SAFETY: no argument, no state changed.
            Self::open(unsafe { GetCurrentThreadId() })
        }

        pub fn open(id: u32) -> Option<Thread> {
            // SAFETY: no borrowed pointers; the rights asked are those of
            // `write_thread_stack`.
            let handle = unsafe { OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION, 0, id) };
            (!handle.is_null()).then_some(Thread(handle))
        }

        pub fn write_stack(&self, out: &mut impl Write) -> bool {
            #[cfg(target_arch = "x86_64")]
            return write_thread_stack(out, self.0);
            #[cfg(not(target_arch = "x86_64"))]
            {
                let _ = out;
                false
            }
        }
    }

    /// Whether Windows considers the window hung: no message taken from its
    /// queue for 5 seconds, the test behind "Not responding".
    pub fn window_hung(window: usize) -> bool {
        // SAFETY: a stale handle only makes the call answer false.
        unsafe { IsHungAppWindow(window as HWND) != 0 }
    }

    /// When Windows last started (Unix seconds), from its uptime.
    pub fn boot_unix() -> Option<u64> {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
        // SAFETY: no argument.
        let up = unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() } / 1000;
        now.checked_sub(up)
    }

    pub fn install(dir: &Path) -> anyhow::Result<()> {
        let file = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("native.txt"))?;
        let _ = TRACE.set(file);
        let oom = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("oom.txt"))?;
        let _ = OOM.set(oom);
        // SAFETY: both callbacks have the ABI Windows expects and only touch
        // static storage that lives until the process ends.
        unsafe {
            SetUnhandledExceptionFilter(Some(exception));
            // First in line: by the time the unhandled filter would run, the
            // standard library's own handler has used part of the little
            // stack the overflowed thread has left.
            AddVectoredExceptionHandler(1, Some(overflow));
        }
        Ok(())
    }

    /// A stack overflow is written as soon as it is raised, on the stack
    /// Windows keeps for it (the standard library asks 20 KB per thread,
    /// `SetThreadStackGuarantee`), then left to end the process as before.
    unsafe extern "system" fn overflow(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: Windows supplies live records for the callback's duration.
        let code = unsafe { info.as_ref().and_then(|i| i.ExceptionRecord.as_ref()).map(|r| r.ExceptionCode) };
        if code == Some(STATUS_STACK_OVERFLOW) {
            // SAFETY: the same live pointers, handed on unchanged.
            unsafe { write_exception(info) };
        }
        EXCEPTION_CONTINUE_SEARCH
    }

    unsafe extern "system" fn exception(info: *const EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the pointers Windows gave this filter, handed on unchanged.
        unsafe { write_exception(info) };
        EXCEPTION_EXECUTE_HANDLER
    }

    /// The exception's code, fault and stack into `native.txt`, once.
    ///
    /// # Safety
    /// `info` is null or the live record Windows passes an exception handler.
    unsafe fn write_exception(info: *const EXCEPTION_POINTERS) {
        if !WRITING.swap(true, Ordering::Relaxed) && !info.is_null() {
            // SAFETY: Windows supplies live exception/context records for the
            // duration of the callback. Check the nested pointer before use.
            let record = unsafe { (*info).ExceptionRecord };
            if let Some(mut file) = TRACE.get()
                && !record.is_null()
            {
                // SAFETY: checked above; formatting only writes to a File and
                // uses fixed stack storage, with no application/logger locks.
                let record = unsafe { &*record };
                let _ = writeln!(
                    file,
                    "Native exception 0x{:08X}\nFault address: {:p}\nNative stack (addresses):",
                    record.ExceptionCode as u32, record.ExceptionAddress
                );
                let mut addresses = [std::ptr::null_mut(); FRAMES];
                // SAFETY: addresses has room for all 64 frames; hash is optional.
                let count = unsafe { RtlCaptureStackBackTrace(0, addresses.len() as u32, addresses.as_mut_ptr(), std::ptr::null_mut()) };
                let frames = &addresses[..usize::from(count)];
                for (index, address) in frames.iter().enumerate() {
                    let _ = writeln!(file, "{index:02}: {:p}", *address);
                }
                let _ = file.sync_all();
                // The module headers are read once the raw stack is on disk:
                // a fault while reading them only loses the module names.
                write_modules(&mut file, frames, Some(record.ExceptionAddress));
                let _ = file.sync_all();
            }
        }
    }

    /// From the allocator, when an allocation fails: its size and the
    /// failing thread's stack, before the standard library aborts the
    /// process (a fast fail no handler sees). No allocation here: the file
    /// was opened at start and the stack goes through fixed arrays. An
    /// allocation that fails without ending the process (`try_reserve`)
    /// writes it too; the report only uses it for a fast fail (summary.rs).
    pub fn out_of_memory(size: usize) {
        if let Some(mut file) = OOM.get()
            && !OOM_WRITING.swap(true, Ordering::Relaxed)
        {
            let _ = writeln!(file, "Out of memory: {size} bytes");
            write_stack(&mut file);
            let _ = file.sync_all();
            OOM_WRITING.store(false, Ordering::Relaxed);
        }
    }

    /// The current thread's stack as module + offset, for a Rust panic.
    pub fn write_stack(out: &mut impl Write) {
        let mut addresses = [std::ptr::null_mut(); FRAMES];
        // SAFETY: addresses has room for all 64 frames; hash is optional.
        let count = unsafe { RtlCaptureStackBackTrace(1, addresses.len() as u32, addresses.as_mut_ptr(), std::ptr::null_mut()) };
        write_modules(out, &addresses[..usize::from(count)], None);
    }

    /// Raw addresses change with ASLR at every launch: only module + offset,
    /// with each module's PDB identity, can be symbolized once the process
    /// is gone. No allocation, no loader lock, no symbol engine: this runs
    /// inside the exception filter.
    pub(super) fn write_modules(out: &mut impl Write, frames: &[*mut c_void], fault: Option<*mut c_void>) {
        let locate = |address: usize| {
            let mut base = std::ptr::null_mut();
            // SAFETY: a lookup in the loader's inverted function table, the
            // one exception dispatch itself has just used to reach this filter.
            unsafe { RtlPcToFileHeader(address as *const c_void, &mut base) };
            base as usize
        };
        // SAFETY: a non-null base came from RtlPcToFileHeader for a frame of
        // this process: the start of an image mapped here, which a module on
        // the stack is not unloaded under.
        let module = |base: usize| unsafe { Module::read(base) };
        write_frames(
            out,
            frames.iter().map(|&frame| frame as usize),
            fault.map(|fault| fault as usize),
            locate,
            module,
        );
    }

    /// The frames as `name+0xoffset`, then each module met with its PDB
    /// identity; `locate` gives an address's module base (0 when none) and
    /// `module` reads that module's headers.
    fn write_frames(
        out: &mut impl Write,
        frames: impl IntoIterator<Item = usize>,
        fault: Option<usize>,
        locate: impl Fn(usize) -> usize,
        module: impl Fn(usize) -> Option<Module>,
    ) {
        let mut bases = [0usize; FRAMES + 1];
        let mut known = 0;
        let mut seen = |base: usize| {
            if base != 0 && !bases[..known].contains(&base) && known < bases.len() {
                bases[known] = base;
                known += 1;
            }
        };
        if let Some(fault) = fault {
            let _ = write!(out, "Fault: ");
            seen(write_location(out, fault, &locate, &module));
        }
        let _ = writeln!(out, "Native stack (modules):");
        for (index, address) in frames.into_iter().enumerate() {
            let _ = write!(out, "{index:02}: ");
            seen(write_location(out, address, &locate, &module));
        }
        let _ = writeln!(out, "Modules:");
        for &base in &bases[..known] {
            let Some(module) = module(base) else { continue };
            let _ = write!(out, "{}", module.name());
            if let Some(guid) = module.guid {
                let data1 = u32::from_le_bytes([guid[0], guid[1], guid[2], guid[3]]);
                let data2 = u16::from_le_bytes([guid[4], guid[5]]);
                let data3 = u16::from_le_bytes([guid[6], guid[7]]);
                let _ = write!(out, " id={data1:08X}{data2:04X}{data3:04X}");
                for byte in &guid[8..] {
                    let _ = write!(out, "{byte:02X}");
                }
                let _ = write!(out, "{:X}", module.age);
            }
            // Code id, as symbol servers key the binary itself.
            let _ = writeln!(out, " code={:08X}{:x}", module.stamp, module.size);
        }
    }

    /// Writes `name+0xoffset` (or `?` outside any module) and a line end;
    /// returns the module base, 0 when unknown.
    fn write_location(
        out: &mut impl Write,
        address: usize,
        locate: &impl Fn(usize) -> usize,
        module: &impl Fn(usize) -> Option<Module>,
    ) -> usize {
        let base = locate(address);
        match (base != 0).then(|| module(base)).flatten() {
            Some(module) => {
                let _ = writeln!(out, "{}+0x{:x}", module.name(), address - base);
                base
            }
            None => {
                let _ = writeln!(out, "?");
                0
            }
        }
    }

    struct Module {
        size: u32,
        stamp: u32,
        // CodeView (RSDS) signature and age: the key of the module's PDB and
        // of the symbol files made from it.
        guid: Option<[u8; 16]>,
        age: u32,
        name: [u8; 64],
        len: usize,
    }

    impl Module {
        fn name(&self) -> &str {
            match std::str::from_utf8(&self.name[..self.len]) {
                Ok(name) if !name.is_empty() => name,
                _ => "module",
            }
        }

        /// Reads the PE headers and the CodeView record where the loader
        /// mapped them, without opening the file.
        ///
        /// # Safety
        /// `base` is the start of an image mapped in this process: its first
        /// page and every range inside SizeOfImage that the headers name are
        /// readable.
        unsafe fn read(base: usize) -> Option<Module> {
            Self::parse(&|offset, buffer: &mut [u8]| {
                // SAFETY: `parse` only asks for ranges inside the first page
                // or inside SizeOfImage, readable as the caller promised.
                unsafe { std::ptr::copy_nonoverlapping((base + offset) as *const u8, buffer.as_mut_ptr(), buffer.len()) };
                true
            })
        }

        /// The same from the watcher, in the viewer's memory: a page it
        /// cannot read only loses that module's name.
        #[cfg(target_arch = "x86_64")]
        fn read_from(process: HANDLE, base: usize) -> Option<Module> {
            Self::parse(&|offset, buffer: &mut [u8]| {
                let mut done = 0;
                // SAFETY: the buffer is ours and as long as asked; the
                // system checks the other process's range itself.
                let read = unsafe {
                    ReadProcessMemory(
                        process,
                        (base + offset) as *const c_void,
                        buffer.as_mut_ptr().cast(),
                        buffer.len(),
                        &mut done,
                    )
                };
                read != 0 && done == buffer.len()
            })
        }

        /// The headers and the CodeView record through `read`, which fills a
        /// buffer from an offset into the image and says whether it could.
        /// Every offset asked lies in the first page or inside SizeOfImage.
        /// No allocation: this also runs inside the exception filter.
        fn parse(read: &impl Fn(usize, &mut [u8]) -> bool) -> Option<Module> {
            const PAGE: usize = 4096;
            let mut word = [0u8; 4];
            if !read(0, &mut word[..2]) || u16::from_le_bytes([word[0], word[1]]) != 0x5A4D || !read(0x3C, &mut word) {
                return None;
            }
            let nt = u32::from_le_bytes(word) as usize;
            if nt + size_of::<IMAGE_NT_HEADERS64>() > PAGE {
                return None;
            }
            // SAFETY: plain data, valid for any bytes.
            let headers = unsafe { plain::<IMAGE_NT_HEADERS64>(read, nt) }?;
            let optional = headers.OptionalHeader;
            if headers.Signature != 0x4550 || optional.Magic != IMAGE_NT_OPTIONAL_HDR64_MAGIC {
                return None;
            }
            let size = optional.SizeOfImage as usize;
            let mut module = Module {
                size: optional.SizeOfImage,
                stamp: headers.FileHeader.TimeDateStamp,
                guid: None,
                age: 0,
                name: [0; 64],
                len: 0,
            };
            let index = IMAGE_DIRECTORY_ENTRY_DEBUG as usize;
            if optional.NumberOfRvaAndSizes as usize <= index {
                return Some(module);
            }
            let directory = optional.DataDirectory[index];
            let (start, length) = (directory.VirtualAddress as usize, directory.Size as usize);
            if start == 0 || start + length > size {
                return Some(module);
            }
            let entry = size_of::<IMAGE_DEBUG_DIRECTORY>();
            for i in 0..(length / entry).min(16) {
                // SAFETY: plain data, valid for any bytes; inside the debug
                // directory, itself inside the image.
                let Some(debug) = (unsafe { plain::<IMAGE_DEBUG_DIRECTORY>(read, start + i * entry) }) else {
                    continue;
                };
                let (data, length) = (debug.AddressOfRawData as usize, debug.SizeOfData as usize);
                if debug.Type != IMAGE_DEBUG_TYPE_CODEVIEW || data == 0 || length < 24 || data + length > size {
                    continue;
                }
                // RSDS, GUID, age, then the PDB path (MAX_PATH at most).
                let mut record = [0u8; 24 + 260];
                let length = length.min(record.len());
                if !read(data, &mut record[..length]) {
                    continue;
                }
                let record = &record[..length];
                if &record[..4] != b"RSDS" {
                    continue;
                }
                module.guid = record[4..20].try_into().ok();
                module.age = u32::from_le_bytes([record[20], record[21], record[22], record[23]]);
                let path = &record[24..];
                let path = &path[..path.iter().position(|&b| b == 0).unwrap_or(path.len())];
                // Only the file name: a local build's PDB path names the
                // developer's folders.
                let file = &path[path.iter().rposition(|&b| b == b'\\' || b == b'/').map_or(0, |i| i + 1)..];
                let stem = match file.len().checked_sub(4) {
                    Some(dot) if file[dot..].eq_ignore_ascii_case(b".pdb") => &file[..dot],
                    _ => file,
                };
                for &byte in stem.iter().take(module.name.len()) {
                    let allowed = byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.');
                    module.name[module.len] = if allowed { byte } else { b'_' };
                    module.len += 1;
                }
                break;
            }
            Some(module)
        }
    }

    /// A `T` read through `read` at `offset`.
    ///
    /// # Safety
    /// Every bit pattern is a valid `T` (the PE header structures).
    unsafe fn plain<T: Copy>(read: &impl Fn(usize, &mut [u8]) -> bool, offset: usize) -> Option<T> {
        let mut value = std::mem::MaybeUninit::<T>::zeroed();
        // SAFETY: the bytes of a zeroed local, as long as T.
        let bytes = unsafe { std::slice::from_raw_parts_mut(value.as_mut_ptr().cast::<u8>(), size_of::<T>()) };
        // SAFETY: filled by `read` or left zeroed, either valid (see above).
        read(offset, bytes).then(|| unsafe { value.assume_init() })
    }

    pub fn demo_exception() -> ! {
        // SAFETY: an intentional, noncontinuable SEH exception in the offline
        // test process; no invalid Rust pointer / undefined behavior involved.
        unsafe { RaiseException(0xE0424155, 1, 0, std::ptr::null()) };
        std::process::abort()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn modules(text: &str) -> Vec<&str> {
            text.split_once("Modules:\n")
                .map(|(_, list)| list.lines().collect())
                .unwrap_or_default()
        }

        #[test]
        fn stacks_are_written_as_module_offsets_with_symbol_identities() {
            let mut out = Vec::new();
            write_stack(&mut out);
            let text = String::from_utf8(out).unwrap();
            let frames: Vec<_> = text.lines().filter(|line| line.contains("+0x")).collect();
            assert!(frames.len() > 3, "{text}");
            // Offsets inside their module, never absolute addresses: those
            // differ at every launch.
            for frame in &frames {
                let offset = frame.rsplit_once("+0x").unwrap().1;
                assert!(u64::from_str_radix(offset, 16).unwrap() < 1 << 32, "{frame}");
            }
            let modules = modules(&text);
            assert!(modules.iter().any(|line| line.starts_with("ntdll ")), "{text}");
            // This test binary has a PDB: GUID (32 hex digits) and age.
            let own = modules.iter().find(|line| line.starts_with("aurora_viewer")).expect("own module");
            let id = own.split(' ').find_map(|part| part.strip_prefix("id=")).expect("debug id");
            assert!(id.len() >= 33 && id.chars().all(|c| c.is_ascii_hexdigit()), "{own}");
            assert!(own.contains(" code="), "{own}");
        }

        #[test]
        fn the_fault_location_and_unknown_frames_are_written() {
            let here = stacks_are_written_as_module_offsets_with_symbol_identities as *mut c_void;
            let mut out = Vec::new();
            write_modules(&mut out, &[here, std::ptr::null_mut()], Some(here));
            let text = String::from_utf8(out).unwrap();
            let mut lines = text.lines();
            let fault = lines.next().unwrap();
            assert!(fault.starts_with("Fault: aurora_viewer") && fault.contains("+0x"), "{text}");
            assert_eq!(lines.next(), Some("Native stack (modules):"));
            assert_eq!(lines.next(), Some(&*format!("00: {}", &fault["Fault: ".len()..])));
            assert_eq!(lines.next(), Some("01: ?"));
            assert_eq!(modules(&text).len(), 1, "{text}");
        }

        #[test]
        fn a_blocked_thread_s_stack_is_read_from_another_thread() {
            let (ids, id) = std::sync::mpsc::channel();
            let (release, wait) = std::sync::mpsc::channel::<()>();
            let blocked = std::thread::Builder::new()
                .name("aurora-crash-test-blocked".into())
                .spawn(move || {
                    // SAFETY: no argument.
                    ids.send(unsafe { GetCurrentThreadId() }).unwrap();
                    let _ = wait.recv();
                })
                .unwrap();
            let id = id.recv().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(50));
            let thread = Thread::open(id).expect("thread handle");
            let mut out = Vec::new();
            assert!(thread.write_stack(&mut out));
            assert!(named_threads().contains(&(id, "aurora-crash-test-blocked".into())));
            release.send(()).unwrap();
            blocked.join().unwrap();
            let text = String::from_utf8(out).unwrap();
            let frames: Vec<_> = text.lines().filter(|line| line.contains("+0x")).collect();
            // From the wait in the system down to the thread's start, through
            // this test binary's own code.
            assert!(frames.len() > 4, "{text}");
            assert!(frames.iter().any(|f| f.contains("aurora_viewer")), "{text}");
            assert!(modules(&text).iter().any(|line| line.starts_with("ntdll ")), "{text}");
        }

        #[cfg(target_arch = "x86_64")]
        #[test]
        fn the_watcher_reads_a_named_thread_s_stack_through_the_debug_help_library() {
            let (ids, id) = std::sync::mpsc::channel();
            let (release, wait) = std::sync::mpsc::channel::<()>();
            let blocked = std::thread::Builder::new()
                .name("aurora-crash-test-outside".into())
                .spawn(move || {
                    // SAFETY: no argument.
                    ids.send(unsafe { GetCurrentThreadId() }).unwrap();
                    let _ = wait.recv();
                })
                .unwrap();
            id.recv().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(50));
            // SAFETY: no argument.
            let pid = unsafe { windows_sys::Win32::System::Threading::GetCurrentProcessId() };
            let mut out = Vec::new();
            // no window: only the named thread
            let read = write_outside_stacks(&mut out, pid, 0, &["aurora-crash-test-outside"]);
            release.send(()).unwrap();
            blocked.join().unwrap();
            let text = String::from_utf8(out).unwrap();
            assert!(read, "{text}");
            assert!(
                text.starts_with("Thread: aurora-crash-test-outside\nNative stack (modules):\n"),
                "{text}"
            );
            let frames: Vec<_> = text.lines().filter(|line| line.contains("+0x")).collect();
            assert!(frames.len() > 4, "{text}");
            assert!(frames.iter().any(|f| f.contains("aurora_viewer")), "{text}");
            let own = modules(&text)
                .into_iter()
                .find(|line| line.starts_with("aurora_viewer"))
                .expect("own module");
            assert!(own.contains(" id=") && own.contains(" code="), "{own}");
        }
    }
}

#[cfg(target_os = "linux")]
mod identity {
    fn read(pid: u32) -> Option<super::Stat> {
        super::parse_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
    }

    /// The start time in clock ticks since boot: with the PID, it names this
    /// process and not a later one given the same number.
    pub fn current_process_started() -> u64 {
        read(std::process::id()).map_or(0, |stat| stat.started)
    }

    /// No watcher waits on the viewer here (it would outlive a closed
    /// terminal or a killed session badly): the next launch asks the kernel.
    /// A session written without a start time (older builds) is alive while
    /// its PID is, so another instance is never taken for a crash.
    pub fn is_alive(pid: u32, creation: u64) -> bool {
        read(pid).is_some_and(|stat| !stat.zombie && (creation == 0 || stat.started == creation))
    }

    /// Without a start time, a dead PID may have been a crash, a kill, or a
    /// viewer still running under a reused PID: never offered as a crash.
    pub fn identifies(creation: u64) -> bool {
        creation != 0
    }
}

#[cfg(target_os = "linux")]
pub use identity::*;

#[cfg(not(any(windows, target_os = "linux")))]
mod identity {
    pub fn current_process_started() -> u64 {
        0
    }
    pub fn is_alive(_pid: u32, _creation: u64) -> bool {
        // No process identity is read on this system yet. Do not
        // misidentify another live instance as crashed.
        true
    }
    pub fn identifies(_creation: u64) -> bool {
        false
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
pub use identity::*;

/// The fields of `/proc/<pid>/stat` that identify a process.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, PartialEq)]
struct Stat {
    zombie: bool,
    started: u64,
}

/// `pid (comm) state ppid … starttime …` (proc_pid_stat(5)): the name may
/// hold spaces and parentheses, so the fields are counted after its last
/// `)`. `state` is field 3, `starttime` field 22.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_stat(text: &str) -> Option<Stat> {
    let mut fields = text.get(text.rfind(')')? + 1..)?.split_ascii_whitespace();
    let state = fields.next()?;
    let started = fields.nth(18)?.parse().ok()?;
    Some(Stat {
        zombie: matches!(state, "Z" | "X" | "x"),
        started,
    })
}

/// The desktop ending the session (log out, shutdown) stops programs with
/// SIGTERM, a closed terminal with SIGHUP, Ctrl+C with SIGINT: the viewer
/// did not crash. The signal leaves the session's `system-ending` marker,
/// as Windows' watcher does when the system ends, then ends the process
/// exactly as before. The Legacy Viewer quits on these signals
/// (LLApp::setQuitting from default_unix_signal_handler,
/// indra/llcommon/llapp.cpp).
#[cfg(target_os = "linux")]
mod ending {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStringExt;
    use std::path::Path;
    use std::sync::OnceLock;

    static MARKER: OnceLock<CString> = OnceLock::new();

    pub fn install(dir: &Path) -> anyhow::Result<()> {
        // Built here: the handler may not allocate.
        let _ = MARKER.set(CString::new(dir.join("system-ending").into_os_string().into_vec())?);
        for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
            // SAFETY: plain sigaction calls on zeroed structs we own; the
            // handler only uses async-signal-safe calls.
            unsafe {
                let mut old: libc::sigaction = std::mem::zeroed();
                anyhow::ensure!(
                    libc::sigaction(signal, std::ptr::null(), &mut old) == 0,
                    "cannot read signal {signal}"
                );
                // A signal the parent ignores (nohup) stays ignored.
                if old.sa_sigaction == libc::SIG_IGN {
                    continue;
                }
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
                action.sa_flags = libc::SA_RESETHAND;
                libc::sigemptyset(&mut action.sa_mask);
                anyhow::ensure!(
                    libc::sigaction(signal, &action, std::ptr::null_mut()) == 0,
                    "cannot handle signal {signal}"
                );
            }
        }
        Ok(())
    }

    extern "C" fn on_signal(signal: libc::c_int) {
        if let Some(path) = MARKER.get() {
            // SAFETY: open and close are async-signal-safe; the path is a
            // C string that lives as long as the process.
            unsafe {
                let file = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CREAT | libc::O_CLOEXEC, 0o600);
                if file >= 0 {
                    libc::close(file);
                }
            }
        }
        // SA_RESETHAND gave the signal its default action back: it now
        // ends the process as it would have without this handler.
        // SAFETY: raise is async-signal-safe.
        unsafe {
            libc::raise(signal);
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    pub use super::super::linux::{Thread, demo_exception, named_threads, out_of_memory, write_stack};
    use std::path::Path;
    pub fn install(dir: &Path) -> anyhow::Result<()> {
        super::ending::install(dir)?;
        super::super::linux::install(dir)
    }
    pub fn boot_unix() -> Option<u64> {
        None
    }
    pub fn watch(_dir: &Path, _record: &super::super::Record) -> anyhow::Result<()> {
        anyhow::bail!("automatic crash monitoring currently requires Windows")
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    use std::path::Path;
    pub fn install(dir: &Path) -> anyhow::Result<()> {
        let _ = dir;
        Ok(())
    }
    pub fn write_stack(_out: &mut impl std::io::Write) {}
    pub fn out_of_memory(_size: usize) {}
    pub fn boot_unix() -> Option<u64> {
        None
    }
    pub fn watch(_dir: &Path, _record: &super::super::Record) -> anyhow::Result<()> {
        anyhow::bail!("automatic crash monitoring currently requires Windows")
    }
    pub fn demo_exception() -> ! {
        std::process::abort()
    }
}

pub use platform::*;

#[cfg(test)]
mod tests {
    use super::{Stat, parse_stat};

    #[test]
    fn the_start_time_is_read_after_the_last_parenthesis_of_the_name() {
        let tail = "S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 987654 20 21";
        assert_eq!(
            parse_stat(&format!("4242 (aurora-viewer) {tail}")),
            Some(Stat {
                zombie: false,
                started: 987654
            })
        );
        // A process may name itself with spaces and parentheses.
        assert_eq!(parse_stat(&format!("4242 (a) b (c) {tail}")).map(|stat| stat.started), Some(987654));
        assert!(parse_stat(&format!("4242 (x) Z{}", &tail[1..])).is_some_and(|stat| stat.zombie));
        assert_eq!(parse_stat("4242 (aurora-viewer) S 1 2"), None);
        assert_eq!(parse_stat(""), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_is_alive_and_a_reused_pid_is_not() {
        let started = super::current_process_started();
        assert_ne!(started, 0);
        assert!(super::is_alive(std::process::id(), started));
        assert!(!super::is_alive(std::process::id(), started + 1));
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!super::is_alive(pid, 0));
    }
}
