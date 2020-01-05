//
// Sysinfo
//
// Copyright (c) 2015 Guillaume Gomez
//

use sys::component::Component;
use sys::disk::{self, Disk, DiskType};
use sys::ffi;
use sys::network::{self, NetworkData};
use sys::process::*;
use sys::processor::*;

use {DiskExt, ProcessExt, ProcessorExt, RefreshKind, SystemExt};

use std::borrow::Borrow;
use std::cell::{RefCell, UnsafeCell};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::mem::MaybeUninit;
use std::ops::Deref;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{fs, mem, ptr};
use sys::processor;

use libc::{self, c_char, c_int, c_void, size_t, sysconf, _SC_PAGESIZE};

use utils;
use Pid;

use std::process::Command;

use rayon::prelude::*;

struct Syncer(RefCell<Option<HashMap<OsString, DiskType>>>);

impl Syncer {
    fn get_disk_types<F: Fn(&HashMap<OsString, DiskType>) -> Vec<Disk>>(
        &self,
        callback: F,
    ) -> Vec<Disk> {
        if self.0.borrow().is_none() {
            *self.0.borrow_mut() = Some(get_disk_types());
        }
        match &*self.0.borrow() {
            Some(x) => callback(x),
            None => Vec::new(),
        }
    }
}

unsafe impl Sync for Syncer {}

static DISK_TYPES: Syncer = Syncer(RefCell::new(None));

fn get_disk_types() -> HashMap<OsString, DiskType> {
    let mut master_port: ffi::mach_port_t = 0;
    let mut media_iterator: ffi::io_iterator_t = 0;
    let mut ret = HashMap::with_capacity(1);

    unsafe {
        ffi::IOMasterPort(ffi::MACH_PORT_NULL, &mut master_port);

        let matching_dictionary = ffi::IOServiceMatching(b"IOMedia\0".as_ptr() as *const i8);
        let result = ffi::IOServiceGetMatchingServices(
            master_port,
            matching_dictionary,
            &mut media_iterator,
        );
        if result != ffi::KERN_SUCCESS as i32 {
            //println!("Error: IOServiceGetMatchingServices() = {}", result);
            return ret;
        }

        loop {
            let next_media = ffi::IOIteratorNext(media_iterator);
            if next_media == 0 {
                break;
            }
            let mut props = MaybeUninit::<ffi::CFMutableDictionaryRef>::uninit();
            let result = ffi::IORegistryEntryCreateCFProperties(
                next_media,
                props.as_mut_ptr(),
                ffi::kCFAllocatorDefault,
                0,
            );
            let props = props.assume_init();
            if result == ffi::KERN_SUCCESS as i32 && check_value(props, b"Whole\0") {
                let mut name: ffi::io_name_t = mem::zeroed();
                if ffi::IORegistryEntryGetName(next_media, name.as_mut_ptr() as *mut c_char)
                    == ffi::KERN_SUCCESS as i32
                {
                    ret.insert(
                        make_name(&name),
                        if check_value(props, b"RAID\0") {
                            DiskType::Unknown(-1)
                        } else {
                            DiskType::SSD
                        },
                    );
                }
                ffi::CFRelease(props as *mut _);
            }
            ffi::IOObjectRelease(next_media);
        }
        ffi::IOObjectRelease(media_iterator);
    }
    ret
}

/// Structs containing system's information.
pub struct System {
    process_list: HashMap<Pid, Process>,
    mem_total: u64,
    mem_free: u64,
    swap_total: u64,
    swap_free: u64,
    processors: Vec<Processor>,
    page_size_kb: u64,
    temperatures: Vec<Component>,
    connection: Option<ffi::io_connect_t>,
    disks: Vec<Disk>,
    network: NetworkData,
    uptime: u64,
    port: ffi::mach_port_t,
}

impl Drop for System {
    fn drop(&mut self) {
        if let Some(conn) = self.connection {
            unsafe {
                ffi::IOServiceClose(conn);
            }
        }
    }
}

// code from https://github.com/Chris911/iStats
fn get_io_service_connection() -> Option<ffi::io_connect_t> {
    let mut master_port: ffi::mach_port_t = 0;
    let mut iterator: ffi::io_iterator_t = 0;

    unsafe {
        ffi::IOMasterPort(ffi::MACH_PORT_NULL, &mut master_port);

        let matching_dictionary = ffi::IOServiceMatching(b"AppleSMC\0".as_ptr() as *const i8);
        let result =
            ffi::IOServiceGetMatchingServices(master_port, matching_dictionary, &mut iterator);
        if result != ffi::KIO_RETURN_SUCCESS {
            //println!("Error: IOServiceGetMatchingServices() = {}", result);
            return None;
        }

        let device = ffi::IOIteratorNext(iterator);
        ffi::IOObjectRelease(iterator);
        if device == 0 {
            //println!("Error: no SMC found");
            return None;
        }

        let mut conn = 0;
        let result = ffi::IOServiceOpen(device, ffi::mach_task_self(), 0, &mut conn);
        ffi::IOObjectRelease(device);
        if result != ffi::KIO_RETURN_SUCCESS {
            //println!("Error: IOServiceOpen() = {}", result);
            return None;
        }

        Some(conn)
    }
}

unsafe fn get_unchecked_str(cp: *mut u8, start: *mut u8) -> String {
    let len = cp as usize - start as usize;
    let part = Vec::from_raw_parts(start, len, len);
    let tmp = String::from_utf8_unchecked(part.clone());
    mem::forget(part);
    tmp
}

macro_rules! unwrapper {
    ($b:expr, $ret:expr) => {{
        match $b {
            Ok(x) => x,
            _ => return $ret,
        }
    }};
}

unsafe fn check_value(dict: ffi::CFMutableDictionaryRef, key: &[u8]) -> bool {
    let key = ffi::CFStringCreateWithCStringNoCopy(
        ptr::null_mut(),
        key.as_ptr() as *const c_char,
        ffi::kCFStringEncodingMacRoman,
        ffi::kCFAllocatorNull as *mut c_void,
    );
    let ret = ffi::CFDictionaryContainsKey(dict as ffi::CFDictionaryRef, key as *const c_void) != 0
        && *(ffi::CFDictionaryGetValue(dict as ffi::CFDictionaryRef, key as *const c_void)
            as *const ffi::Boolean)
            != 0;
    ffi::CFRelease(key as *const c_void);
    ret
}

fn make_name(v: &[u8]) -> OsString {
    for (pos, x) in v.iter().enumerate() {
        if *x == 0 {
            return OsStringExt::from_vec(v[0..pos].to_vec());
        }
    }
    OsStringExt::from_vec(v.to_vec())
}

fn get_disks() -> Vec<Disk> {
    DISK_TYPES.get_disk_types(|disk_types| {
        unwrapper!(fs::read_dir("/Volumes"), Vec::new())
            .flat_map(|x| {
                if let Ok(ref entry) = x {
                    let mount_point = utils::realpath(&entry.path());
                    if mount_point.as_os_str().is_empty() {
                        None
                    } else {
                        let name = entry.path().file_name()?.to_owned();
                        let type_ = disk_types
                            .get(&name)
                            .cloned()
                            .unwrap_or(DiskType::Unknown(-2));
                        Some(disk::new(name, &mount_point, type_))
                    }
                } else {
                    None
                }
            })
            .collect()
    })
}

fn get_uptime() -> u64 {
    let mut boottime: libc::timeval = unsafe { mem::zeroed() };
    let mut len = mem::size_of::<libc::timeval>();
    let mut mib: [c_int; 2] = [libc::CTL_KERN, libc::KERN_BOOTTIME];
    unsafe {
        if libc::sysctl(
            mib.as_mut_ptr(),
            2,
            &mut boottime as *mut libc::timeval as *mut _,
            &mut len,
            ::std::ptr::null_mut(),
            0,
        ) < 0
        {
            return 0;
        }
    }
    let bsec = boottime.tv_sec;
    let csec = unsafe { libc::time(::std::ptr::null_mut()) };

    unsafe { libc::difftime(csec, bsec) as u64 }
}

fn parse_command_line<T: Deref<Target = str> + Borrow<str>>(cmd: &[T]) -> Vec<String> {
    let mut x = 0;
    let mut command = Vec::with_capacity(cmd.len());
    while x < cmd.len() {
        let mut y = x;
        if cmd[y].starts_with('\'') || cmd[y].starts_with('"') {
            let c = if cmd[y].starts_with('\'') { '\'' } else { '"' };
            while y < cmd.len() && !cmd[y].ends_with(c) {
                y += 1;
            }
            command.push(cmd[x..y].join(" "));
            x = y;
        } else {
            command.push(cmd[x].to_owned());
        }
        x += 1;
    }
    command
}

struct Wrap<'a>(UnsafeCell<&'a mut HashMap<Pid, Process>>);

unsafe impl<'a> Send for Wrap<'a> {}
unsafe impl<'a> Sync for Wrap<'a> {}

fn update_process(
    wrap: &Wrap,
    pid: Pid,
    taskallinfo_size: i32,
    taskinfo_size: i32,
    threadinfo_size: i32,
    mib: &mut [c_int],
    mut size: size_t,
) -> Result<Option<Process>, ()> {
    let mut proc_args = Vec::with_capacity(size as usize);
    unsafe {
        let mut thread_info = mem::zeroed::<libc::proc_threadinfo>();
        let (user_time, system_time, thread_status) = if ffi::proc_pidinfo(
            pid,
            libc::PROC_PIDTHREADINFO,
            0,
            &mut thread_info as *mut libc::proc_threadinfo as *mut c_void,
            threadinfo_size,
        ) != 0
        {
            (
                thread_info.pth_user_time,
                thread_info.pth_system_time,
                Some(ThreadStatus::from(thread_info.pth_run_state)),
            )
        } else {
            (0, 0, None)
        };
        if let Some(ref mut p) = (*wrap.0.get()).get_mut(&pid) {
            if p.memory == 0 {
                // We don't have access to this process' information.
                force_update(p);
                return Ok(None);
            }
            p.status = thread_status;
            let mut task_info = mem::zeroed::<libc::proc_taskinfo>();
            if ffi::proc_pidinfo(
                pid,
                libc::PROC_PIDTASKINFO,
                0,
                &mut task_info as *mut libc::proc_taskinfo as *mut c_void,
                taskinfo_size,
            ) != taskinfo_size
            {
                return Err(());
            }
            let task_time =
                user_time + system_time + task_info.pti_total_user + task_info.pti_total_system;
            let time = ffi::mach_absolute_time();
            compute_cpu_usage(p, time, task_time);

            p.memory = task_info.pti_resident_size >> 10; // divide by 1024
            p.virtual_memory = task_info.pti_virtual_size >> 10; // divide by 1024
            return Ok(None);
        }

        let mut task_info = mem::zeroed::<libc::proc_taskallinfo>();
        if ffi::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKALLINFO,
            0,
            &mut task_info as *mut libc::proc_taskallinfo as *mut c_void,
            taskallinfo_size as i32,
        ) != taskallinfo_size as i32
        {
            match Command::new("/bin/ps") // not very nice, might be worth running a which first.
                .arg("wwwe")
                .arg("-o")
                .arg("ppid=,command=")
                .arg(pid.to_string().as_str())
                .output()
            {
                Ok(o) => {
                    let o = String::from_utf8(o.stdout).unwrap_or_else(|_| String::new());
                    let o = o.split(' ').filter(|c| !c.is_empty()).collect::<Vec<_>>();
                    if o.len() < 2 {
                        return Err(());
                    }
                    let mut command = parse_command_line(&o[1..]);
                    if let Some(ref mut x) = command.last_mut() {
                        **x = x.replace("\n", "");
                    }
                    let p = match i32::from_str_radix(&o[0].replace("\n", ""), 10) {
                        Ok(x) => x,
                        _ => return Err(()),
                    };
                    let exe = PathBuf::from(&command[0]);
                    let name = match exe.file_name() {
                        Some(x) => x.to_str().unwrap_or_else(|| "").to_owned(),
                        None => String::new(),
                    };
                    return Ok(Some(Process::new_with(
                        pid,
                        if p == 0 { None } else { Some(p) },
                        0,
                        exe,
                        name,
                        command,
                    )));
                }
                _ => {
                    return Err(());
                }
            }
        }

        let parent = match task_info.pbsd.pbi_ppid as Pid {
            0 => None,
            p => Some(p),
        };

        let ptr: *mut u8 = proc_args.as_mut_slice().as_mut_ptr();
        mib[0] = libc::CTL_KERN;
        mib[1] = libc::KERN_PROCARGS2;
        mib[2] = pid as c_int;
        /*
         * /---------------\ 0x00000000
         * | ::::::::::::: |
         * |---------------| <-- Beginning of data returned by sysctl() is here.
         * | argc          |
         * |---------------|
         * | exec_path     |
         * |---------------|
         * | 0             |
         * |---------------|
         * | arg[0]        |
         * |---------------|
         * | 0             |
         * |---------------|
         * | arg[n]        |
         * |---------------|
         * | 0             |
         * |---------------|
         * | env[0]        |
         * |---------------|
         * | 0             |
         * |---------------|
         * | env[n]        |
         * |---------------|
         * | ::::::::::::: |
         * |---------------| <-- Top of stack.
         * :               :
         * :               :
         * \---------------/ 0xffffffff
         */
        if libc::sysctl(
            mib.as_mut_ptr(),
            3,
            ptr as *mut c_void,
            &mut size,
            ::std::ptr::null_mut(),
            0,
        ) == -1
        {
            return Err(()); // not enough rights I assume?
        }
        let mut n_args: c_int = 0;
        libc::memcpy(
            (&mut n_args) as *mut c_int as *mut c_void,
            ptr as *const c_void,
            mem::size_of::<c_int>(),
        );

        let mut cp = ptr.add(mem::size_of::<c_int>());
        let mut start = cp;

        let mut p = if cp < ptr.add(size) {
            while cp < ptr.add(size) && *cp != 0 {
                cp = cp.offset(1);
            }
            let exe = Path::new(get_unchecked_str(cp, start).as_str()).to_path_buf();
            let name = exe
                .file_name()
                .unwrap_or_else(|| OsStr::new(""))
                .to_str()
                .unwrap_or_else(|| "")
                .to_owned();
            while cp < ptr.add(size) && *cp == 0 {
                cp = cp.offset(1);
            }
            start = cp;
            let mut c = 0;
            let mut cmd = Vec::with_capacity(n_args as usize);
            while c < n_args && cp < ptr.add(size) {
                if *cp == 0 {
                    c += 1;
                    cmd.push(get_unchecked_str(cp, start));
                    start = cp.offset(1);
                }
                cp = cp.offset(1);
            }

            #[inline]
            fn do_nothing(_: &str, _: &mut PathBuf, _: &mut bool) {}
            #[inline]
            fn do_something(env: &str, root: &mut PathBuf, check: &mut bool) {
                if *check && env.starts_with("PATH=") {
                    *check = false;
                    *root = Path::new(&env[6..]).to_path_buf();
                }
            }

            #[inline]
            unsafe fn get_environ<F: Fn(&str, &mut PathBuf, &mut bool)>(
                ptr: *mut u8,
                mut cp: *mut u8,
                size: size_t,
                mut root: PathBuf,
                callback: F,
            ) -> (Vec<String>, PathBuf) {
                let mut environ = Vec::with_capacity(10);
                let mut start = cp;
                let mut check = true;
                while cp < ptr.add(size) {
                    if *cp == 0 {
                        if cp == start {
                            break;
                        }
                        let e = get_unchecked_str(cp, start);
                        callback(&e, &mut root, &mut check);
                        environ.push(e);
                        start = cp.offset(1);
                    }
                    cp = cp.offset(1);
                }
                (environ, root)
            }

            let (environ, root) = if exe.is_absolute() {
                if let Some(parent) = exe.parent() {
                    get_environ(ptr, cp, size, parent.to_path_buf(), do_nothing)
                } else {
                    get_environ(ptr, cp, size, PathBuf::new(), do_something)
                }
            } else {
                get_environ(ptr, cp, size, PathBuf::new(), do_something)
            };

            Process::new_with2(
                pid,
                parent,
                task_info.pbsd.pbi_start_tvsec,
                exe,
                name,
                parse_command_line(&cmd),
                environ,
                root,
            )
        } else {
            Process::new(pid, parent, task_info.pbsd.pbi_start_tvsec)
        };

        p.memory = task_info.ptinfo.pti_resident_size >> 10; // divide by 1024
        p.virtual_memory = task_info.ptinfo.pti_virtual_size >> 10; // divide by 1024

        p.uid = task_info.pbsd.pbi_uid;
        p.gid = task_info.pbsd.pbi_gid;
        p.process_status = ProcessStatus::from(task_info.pbsd.pbi_status);

        Ok(Some(p))
    }
}

fn get_proc_list() -> Option<Vec<Pid>> {
    let count = unsafe { ffi::proc_listallpids(::std::ptr::null_mut(), 0) };
    if count < 1 {
        return None;
    }
    let mut pids: Vec<Pid> = Vec::with_capacity(count as usize);
    unsafe {
        pids.set_len(count as usize);
    }
    let count = count * mem::size_of::<Pid>() as i32;
    let x = unsafe { ffi::proc_listallpids(pids.as_mut_ptr() as *mut c_void, count) };

    if x < 1 || x as usize >= pids.len() {
        None
    } else {
        unsafe {
            pids.set_len(x as usize);
        }
        Some(pids)
    }
}

fn get_arg_max() -> usize {
    let mut mib: [c_int; 3] = [libc::CTL_KERN, libc::KERN_ARGMAX, 0];
    let mut arg_max = 0i32;
    let mut size = mem::size_of::<c_int>();
    unsafe {
        if libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&mut arg_max) as *mut i32 as *mut c_void,
            &mut size,
            ::std::ptr::null_mut(),
            0,
        ) == -1
        {
            4096 // We default to this value
        } else {
            arg_max as usize
        }
    }
}

unsafe fn get_sys_value(
    high: u32,
    low: u32,
    mut len: usize,
    value: *mut c_void,
    mib: &mut [i32; 2],
) -> bool {
    mib[0] = high as i32;
    mib[1] = low as i32;
    libc::sysctl(
        mib.as_mut_ptr(),
        2,
        value,
        &mut len as *mut usize,
        ::std::ptr::null_mut(),
        0,
    ) == 0
}

impl System {
    fn clear_procs(&mut self) {
        let mut to_delete = Vec::new();

        for (pid, mut proc_) in &mut self.process_list {
            if !has_been_updated(&mut proc_) {
                to_delete.push(*pid);
            }
        }
        for pid in to_delete {
            self.process_list.remove(&pid);
        }
    }
}

impl SystemExt for System {
    fn new_with_specifics(refreshes: RefreshKind) -> System {
        let mut s = System {
            process_list: HashMap::with_capacity(200),
            mem_total: 0,
            mem_free: 0,
            swap_total: 0,
            swap_free: 0,
            processors: Vec::with_capacity(4),
            page_size_kb: unsafe { sysconf(_SC_PAGESIZE) as u64 >> 10 }, // divide by 1024
            temperatures: Vec::with_capacity(2),
            connection: get_io_service_connection(),
            disks: Vec::with_capacity(1),
            network: network::new(),
            uptime: get_uptime(),
            port: unsafe { ffi::mach_host_self() },
        };
        s.refresh_specifics(refreshes);
        s
    }

    fn refresh_memory(&mut self) {
        let mut mib = [0, 0];

        self.uptime = get_uptime();
        unsafe {
            // get system values
            // get swap info
            let mut xs: ffi::xsw_usage = mem::zeroed::<ffi::xsw_usage>();
            if get_sys_value(
                ffi::CTL_VM,
                ffi::VM_SWAPUSAGE,
                mem::size_of::<ffi::xsw_usage>(),
                &mut xs as *mut ffi::xsw_usage as *mut c_void,
                &mut mib,
            ) {
                self.swap_total = xs.xsu_total >> 10; // divide by 1024
                self.swap_free = xs.xsu_avail >> 10; // divide by 1024
            }
            // get ram info
            if self.mem_total < 1 {
                get_sys_value(
                    ffi::CTL_HW,
                    ffi::HW_MEMSIZE,
                    mem::size_of::<u64>(),
                    &mut self.mem_total as *mut u64 as *mut c_void,
                    &mut mib,
                );
                self.mem_total >>= 10; // divide by 1024
            }
            let count: u32 = ffi::HOST_VM_INFO64_COUNT;
            let mut stat = mem::zeroed::<ffi::vm_statistics64>();
            if ffi::host_statistics64(
                self.port,
                ffi::HOST_VM_INFO64,
                &mut stat as *mut ffi::vm_statistics64 as *mut c_void,
                &count,
            ) == ffi::KERN_SUCCESS
            {
                // From the apple documentation:
                //
                // /*
                //  * NB: speculative pages are already accounted for in "free_count",
                //  * so "speculative_count" is the number of "free" pages that are
                //  * used to hold data that was read speculatively from disk but
                //  * haven't actually been used by anyone so far.
                //  */
                // self.mem_free = u64::from(stat.free_count) * self.page_size_kb;
                self.mem_free = self.mem_total
                    - (u64::from(stat.active_count)
                        + u64::from(stat.inactive_count)
                        + u64::from(stat.wire_count)
                        + u64::from(stat.speculative_count)
                        - u64::from(stat.purgeable_count))
                        * self.page_size_kb;
            }
        }
    }

    fn refresh_temperatures(&mut self) {
        if let Some(con) = self.connection {
            if self.temperatures.is_empty() {
                // getting CPU critical temperature
                let critical_temp = crate::mac::component::get_temperature(
                    con,
                    &['T' as i8, 'C' as i8, '0' as i8, 'D' as i8, 0],
                );

                for (id, v) in crate::mac::component::COMPONENTS_TEMPERATURE_IDS.iter() {
                    if let Some(c) = Component::new(id.to_string(), None, critical_temp, v, con) {
                        self.temperatures.push(c);
                    }
                }
            } else {
                for comp in &mut self.temperatures {
                    comp.update(con);
                }
            }
        }
    }

    fn refresh_cpu(&mut self) {
        self.uptime = get_uptime();

        let mut mib = [0, 0];
        unsafe {
            // get processor values
            let mut num_cpu_u = 0u32;
            let mut cpu_info: *mut i32 = ::std::ptr::null_mut();
            let mut num_cpu_info = 0u32;

            if self.processors.is_empty() {
                let mut num_cpu = 0;

                if !get_sys_value(
                    ffi::CTL_HW,
                    ffi::HW_NCPU,
                    mem::size_of::<u32>(),
                    &mut num_cpu as *mut usize as *mut c_void,
                    &mut mib,
                ) {
                    num_cpu = 1;
                }

                self.processors.push(processor::create_proc(
                    "0".to_owned(),
                    Arc::new(ProcessorData::new(::std::ptr::null_mut(), 0)),
                ));
                if ffi::host_processor_info(
                    self.port,
                    ffi::PROCESSOR_CPU_LOAD_INFO,
                    &mut num_cpu_u as *mut u32,
                    &mut cpu_info as *mut *mut i32,
                    &mut num_cpu_info as *mut u32,
                ) == ffi::KERN_SUCCESS
                {
                    let proc_data = Arc::new(ProcessorData::new(cpu_info, num_cpu_info));
                    for i in 0..num_cpu {
                        let mut p =
                            processor::create_proc(format!("{}", i + 1), Arc::clone(&proc_data));
                        let in_use = *cpu_info.offset(
                            (ffi::CPU_STATE_MAX * i) as isize + ffi::CPU_STATE_USER as isize,
                        ) + *cpu_info.offset(
                            (ffi::CPU_STATE_MAX * i) as isize + ffi::CPU_STATE_SYSTEM as isize,
                        ) + *cpu_info.offset(
                            (ffi::CPU_STATE_MAX * i) as isize + ffi::CPU_STATE_NICE as isize,
                        );
                        let total = in_use
                            + *cpu_info.offset(
                                (ffi::CPU_STATE_MAX * i) as isize + ffi::CPU_STATE_IDLE as isize,
                            );
                        processor::set_cpu_proc(&mut p, in_use as f32 / total as f32);
                        self.processors.push(p);
                    }
                }
            } else if ffi::host_processor_info(
                self.port,
                ffi::PROCESSOR_CPU_LOAD_INFO,
                &mut num_cpu_u as *mut u32,
                &mut cpu_info as *mut *mut i32,
                &mut num_cpu_info as *mut u32,
            ) == ffi::KERN_SUCCESS
            {
                let mut pourcent = 0f32;
                let proc_data = Arc::new(ProcessorData::new(cpu_info, num_cpu_info));
                for (i, proc_) in self.processors.iter_mut().skip(1).enumerate() {
                    let old_proc_data = &*processor::get_processor_data(proc_);
                    let in_use =
                        (*cpu_info.offset(
                            (ffi::CPU_STATE_MAX * i) as isize + ffi::CPU_STATE_USER as isize,
                        ) - *old_proc_data.cpu_info.offset(
                            (ffi::CPU_STATE_MAX * i) as isize + ffi::CPU_STATE_USER as isize,
                        )) + (*cpu_info.offset(
                            (ffi::CPU_STATE_MAX * i) as isize + ffi::CPU_STATE_SYSTEM as isize,
                        ) - *old_proc_data.cpu_info.offset(
                            (ffi::CPU_STATE_MAX * i) as isize + ffi::CPU_STATE_SYSTEM as isize,
                        )) + (*cpu_info.offset(
                            (ffi::CPU_STATE_MAX * i) as isize + ffi::CPU_STATE_NICE as isize,
                        ) - *old_proc_data.cpu_info.offset(
                            (ffi::CPU_STATE_MAX * i) as isize + ffi::CPU_STATE_NICE as isize,
                        ));
                    let total = in_use
                        + (*cpu_info.offset(
                            (ffi::CPU_STATE_MAX * i) as isize + ffi::CPU_STATE_IDLE as isize,
                        ) - *old_proc_data.cpu_info.offset(
                            (ffi::CPU_STATE_MAX * i) as isize + ffi::CPU_STATE_IDLE as isize,
                        ));
                    processor::update_proc(
                        proc_,
                        in_use as f32 / total as f32,
                        Arc::clone(&proc_data),
                    );
                    pourcent += proc_.get_cpu_usage();
                }
                if self.processors.len() > 1 {
                    let len = self.processors.len() - 1;
                    if let Some(p) = self.processors.get_mut(0) {
                        processor::set_cpu_usage(p, pourcent / len as f32);
                    }
                }
            }
        }
    }

    fn refresh_network(&mut self) {
        network::update_network(&mut self.network);
    }

    fn refresh_processes(&mut self) {
        let count = unsafe { ffi::proc_listallpids(::std::ptr::null_mut(), 0) };
        if count < 1 {
            return;
        }
        if let Some(pids) = get_proc_list() {
            let taskallinfo_size = mem::size_of::<libc::proc_taskallinfo>() as i32;
            let taskinfo_size = mem::size_of::<libc::proc_taskinfo>() as i32;
            let threadinfo_size = mem::size_of::<libc::proc_threadinfo>() as i32;
            let arg_max = get_arg_max();

            let entries: Vec<Process> = {
                let wrap = &Wrap(UnsafeCell::new(&mut self.process_list));
                pids.par_iter()
                    .flat_map(|pid| {
                        let mut mib: [c_int; 3] = [libc::CTL_KERN, libc::KERN_ARGMAX, 0];
                        match update_process(
                            wrap,
                            *pid,
                            taskallinfo_size,
                            taskinfo_size,
                            threadinfo_size,
                            &mut mib,
                            arg_max as size_t,
                        ) {
                            Ok(x) => x,
                            Err(_) => None,
                        }
                    })
                    .collect()
            };
            entries.into_iter().for_each(|entry| {
                self.process_list.insert(entry.pid(), entry);
            });
            self.clear_procs();
        }
    }

    fn refresh_process(&mut self, pid: Pid) -> bool {
        let taskallinfo_size = mem::size_of::<libc::proc_taskallinfo>() as i32;
        let taskinfo_size = mem::size_of::<libc::proc_taskinfo>() as i32;
        let threadinfo_size = mem::size_of::<libc::proc_threadinfo>() as i32;

        let mut mib: [c_int; 3] = [libc::CTL_KERN, libc::KERN_ARGMAX, 0];
        let arg_max = get_arg_max();
        match {
            let wrap = Wrap(UnsafeCell::new(&mut self.process_list));
            update_process(
                &wrap,
                pid,
                taskallinfo_size,
                taskinfo_size,
                threadinfo_size,
                &mut mib,
                arg_max as size_t,
            )
        } {
            Ok(Some(p)) => {
                self.process_list.insert(p.pid(), p);
                true
            }
            Ok(_) => true,
            Err(_) => false,
        }
    }

    fn refresh_disks(&mut self) {
        for disk in &mut self.disks {
            disk.update();
        }
    }

    fn refresh_disk_list(&mut self) {
        self.disks = get_disks();
    }

    // COMMON PART
    //
    // Need to be moved into a "common" file to avoid duplication.

    fn get_process_list(&self) -> &HashMap<Pid, Process> {
        &self.process_list
    }

    fn get_process(&self, pid: Pid) -> Option<&Process> {
        self.process_list.get(&pid)
    }

    fn get_processor_list(&self) -> &[Processor] {
        &self.processors[..]
    }

    fn get_network(&self) -> &NetworkData {
        &self.network
    }

    fn get_total_memory(&self) -> u64 {
        self.mem_total
    }

    fn get_free_memory(&self) -> u64 {
        self.mem_free
    }

    fn get_used_memory(&self) -> u64 {
        self.mem_total - self.mem_free
    }

    fn get_total_swap(&self) -> u64 {
        self.swap_total
    }

    fn get_free_swap(&self) -> u64 {
        self.swap_free
    }

    // need to be checked
    fn get_used_swap(&self) -> u64 {
        self.swap_total - self.swap_free
    }

    fn get_components_list(&self) -> &[Component] {
        &self.temperatures[..]
    }

    fn get_disks(&self) -> &[Disk] {
        &self.disks[..]
    }

    fn get_uptime(&self) -> u64 {
        self.uptime
    }
}

impl Default for System {
    fn default() -> System {
        System::new()
    }
}
