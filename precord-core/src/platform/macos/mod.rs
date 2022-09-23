use crate::Pid;
use core_foundation::base::{kCFAllocatorDefault, CFRelease, ToVoid};
use core_foundation::dictionary::{CFDictionaryGetValueIfPresent, CFMutableDictionaryRef};
use core_foundation::number::{kCFNumberCharType, CFNumberGetValue, CFNumberRef};
use core_foundation::string::CFString;
use serde::Deserialize;
use std::ffi::c_void;
use std::io::{BufRead, BufReader, Cursor};
use std::mem::MaybeUninit;
use std::process::Command;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{mpsc, Once};
use std::time::Instant;
use std::{mem, process, ptr, thread};
use IOKit_sys::*;

#[allow(dead_code)]
#[allow(non_camel_case_types)]
#[allow(non_snake_case)]
#[allow(non_upper_case_globals)]
mod sysctl;
mod top;

#[derive(Debug, Default, Deserialize)]
struct PowerMetricsResult {
    #[allow(dead_code)]
    tasks: Vec<Task>,
    processor: ProcessorInfo,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct Task {
    pid: Pid,
    #[serde(default)]
    gputime_ms_per_s: f32,
}

pub struct CommandSource {
    last_update: Instant,
    power_metrics_result: PowerMetricsResult,
    process_command_result: Vec<ProcessCommandResult>,
    process_command_rx: Receiver<ProcessCommandResult>,
}

impl CommandSource {
    pub fn new<T: IntoIterator<Item = Pid> + Clone>(
        pids: T,
        net_traffic: bool,
        frame_rate: bool,
        top: bool,
    ) -> Self {
        let pids: Vec<_> = pids.into_iter().collect();

        let process_command_result: Vec<_> = pids
            .iter()
            .copied()
            .map(|pid| ProcessCommandResult {
                pid,
                ..Default::default()
            })
            .collect();

        let (tx, rx) = mpsc::channel();

        // Net traffic
        if net_traffic && !pids.is_empty() {
            let net_top_runner = NetTopRunner::new(tx.clone());
            let pids = pids.clone();
            thread::spawn(move || {
                net_top_runner.run(pids);
            });
        };

        // Frame rate
        if frame_rate && !pids.is_empty() {
            let frame_rate = FrameRateRunner::new(tx.clone());
            let pids = pids.clone();
            thread::spawn(move || frame_rate.run(pids));
        };

        // Top
        if top && !pids.is_empty() {
            let top_runner = top::TopRunner::new(tx);
            thread::spawn(move || top_runner.run(pids));
        }

        Self {
            last_update: Instant::now(),
            power_metrics_result: Default::default(),
            process_command_result,
            process_command_rx: rx,
        }
    }

    pub fn update_power_metrics_data(&mut self) {
        let o = Command::new("powermetrics")
            .args([
                "--samplers",
                "tasks,cpu_power",
                "--show-process-gpu",
                "-n1",
                "-i1000",
                "-f",
                "plist",
            ])
            .output()
            .unwrap();

        match o.status.code() {
            Some(0) => {}
            _ => {
                panic!("Error: {}", String::from_utf8_lossy(&o.stderr));
            }
        }

        self.power_metrics_result = plist::from_bytes(o.stdout.as_slice()).unwrap();
    }

    pub fn update(&mut self) {
        while let Ok(p_result) = self.process_command_rx.try_recv() {
            if let Some(p) = self
                .process_command_result
                .iter_mut()
                .find(|p| p.pid == p_result.pid)
            {
                p.bytes_in += p_result.bytes_in;
                p.bytes_out += p_result.bytes_out;
                p.frame += p_result.frame;
                p.mach_ports = p_result.mach_ports;
            }
        }
        let now = Instant::now();
        let d = (now - self.last_update).as_secs_f32();
        for p in self.process_command_result.iter_mut() {
            p.bytes_in_per_sec = (p.bytes_in as f32 / d) as _;
            p.bytes_out_per_sec = (p.bytes_out as f32 / d) as _;
            p.bytes_in = 0;
            p.bytes_out = 0;

            p.frame_per_sec = p.frame as f32 / d;
            p.frame = 0;
        }
        self.last_update = now;
    }

    pub fn cpu_frequency(&self) -> Vec<f32> {
        if !self.power_metrics_result.processor.clusters.is_empty() {
            // Apple Silicon
            self.power_metrics_result
                .processor
                .clusters
                .iter()
                .flat_map(|c| c.cpus.iter())
                .map(|c| c.freq_hz / 1_000_000.0)
                .collect()
        } else {
            // Intel
            self.power_metrics_result
                .processor
                .packages
                .iter()
                .flat_map(|p| p.cores.iter())
                .flat_map(|c| c.cpus.iter())
                .map(|c| c.freq_hz / 1_000_000.0)
                .collect()
        }
    }

    pub fn process_net_traffic_in(&self, pid: Pid) -> Option<u32> {
        self.process_command_result
            .iter()
            .find(|p| p.pid == pid)
            .map(|p| p.bytes_in_per_sec)
    }

    pub fn process_net_traffic_out(&self, pid: Pid) -> Option<u32> {
        self.process_command_result
            .iter()
            .find(|p| p.pid == pid)
            .map(|p| p.bytes_out_per_sec)
    }

    pub fn process_frame_per_sec(&self, pid: Pid) -> Option<f32> {
        self.process_command_result
            .iter()
            .find(|p| p.pid == pid)
            .map(|p| p.frame_per_sec)
    }

    pub fn process_mach_ports(&self, pid: Pid) -> Option<u32> {
        self.process_command_result
            .iter()
            .find(|p| p.pid == pid)
            .map(|p| p.mach_ports)
    }
}

#[derive(Debug, Default, Deserialize)]
struct ProcessorInfo {
    #[serde(default)]
    clusters: Vec<Cluster>,
    #[serde(default)]
    packages: Vec<Package>,
}

#[derive(Debug, Deserialize)]
struct Cpu {
    freq_hz: f32,
}

// Apple Silicon
#[derive(Debug, Deserialize)]
struct Cluster {
    cpus: Vec<Cpu>,
}

// Intel
#[derive(Debug, Deserialize)]
struct Package {
    cores: Vec<Core>,
}

#[derive(Debug, Deserialize)]
struct Core {
    cpus: Vec<Cpu>,
}

pub struct IOKitRegistry {
    last_result: Vec<IOKitResult>,
}

impl IOKitRegistry {
    pub fn new() -> Self {
        Self {
            last_result: vec![],
        }
    }

    pub fn poll(&mut self) {
        self.last_result.clear();

        unsafe {
            let io_acc = IOServiceMatching("IOAccelerator\0".as_ptr() as _);
            let mut it: io_iterator_t = 0;
            if IOServiceGetMatchingServices(kIOMasterPortDefault, io_acc, &mut it)
                == kIOReturnSuccess
            {
                #[allow(unused_assignments)]
                let mut entry: io_registry_entry_t = 0;
                loop {
                    entry = IOIteratorNext(it);
                    if entry == 0 {
                        break;
                    }

                    let mut props: CFMutableDictionaryRef = std::ptr::null_mut();
                    if IORegistryEntryCreateCFProperties(
                        entry,
                        std::mem::transmute(&mut props),
                        std::mem::transmute(kCFAllocatorDefault),
                        0,
                    ) == kIOReturnSuccess
                    {
                        let mut perf_props: CFMutableDictionaryRef = std::ptr::null_mut();
                        if CFDictionaryGetValueIfPresent(
                            props,
                            CFString::new("PerformanceStatistics").to_void(),
                            std::mem::transmute(&mut perf_props),
                        ) != 0
                        {
                            let mut device_utilization_ref: CFNumberRef = std::ptr::null_mut();
                            if CFDictionaryGetValueIfPresent(
                                perf_props,
                                CFString::new("Device Utilization %").to_void(),
                                std::mem::transmute(&mut device_utilization_ref),
                            ) != 0
                            {
                                let mut device_utilization: u8 = 0;
                                if CFNumberGetValue(
                                    device_utilization_ref,
                                    kCFNumberCharType,
                                    std::mem::transmute(&mut device_utilization),
                                ) {
                                    self.last_result.push(IOKitResult {
                                        // TODO: Get accelerator name
                                        io_class: "".to_string(),
                                        performance_statistics: PerformanceStatistics {
                                            device_utilization: device_utilization as _,
                                        },
                                    });
                                }
                            }
                        }
                        CFRelease(props.to_void());
                    }

                    IOObjectRelease(entry);
                }
                IOObjectRelease(it);
            } else {
                CFRelease(io_acc as _);
            }
        }
    }

    pub fn sys_gpu_usage(&self) -> f32 {
        let mut max: f32 = 0.0;
        for r in &self.last_result {
            max = max.max(r.performance_statistics.device_utilization);
        }
        max
    }
}

#[derive(Debug, Deserialize)]
struct IOKitResult {
    #[allow(dead_code)]
    #[serde(rename = "IOClass")]
    io_class: String,
    #[serde(rename = "PerformanceStatistics")]
    performance_statistics: PerformanceStatistics,
}

#[derive(Debug, Deserialize)]
struct PerformanceStatistics {
    #[serde(rename = "Device Utilization %", default)]
    device_utilization: f32,
}

#[derive(Default)]
pub struct ProcessCommandResult {
    pid: Pid,
    bytes_in: u32,
    bytes_in_per_sec: u32,
    bytes_out: u32,
    bytes_out_per_sec: u32,
    frame: u32,
    frame_per_sec: f32,
    mach_ports: u32,
}

struct NetTopRunner {
    tx: Sender<ProcessCommandResult>,
}

impl NetTopRunner {
    fn new(tx: Sender<ProcessCommandResult>) -> Self {
        Self { tx }
    }

    fn run<T: IntoIterator<Item = Pid>>(self, pids: T) {
        let mut command = Command::new("script");
        command.args([
            "-q",
            "/dev/null",
            "nettop",
            "-P",
            "-d",
            "-L",
            "0",
            "-J",
            "bytes_in,bytes_out",
        ]);

        for p in pids {
            command.args(["-p", p.to_string().as_str()]);
        }

        let mut child = command
            .stdin(process::Stdio::piped())
            .stdout(process::Stdio::piped())
            .spawn()
            .unwrap();

        let mut buf = BufReader::new(child.stdout.as_mut().unwrap());
        let mut line = String::new();
        let mut session_index = 0;
        while let Ok(read) = buf.read_line(&mut line) {
            if read == 0 {
                break;
            }

            if line.starts_with(",bytes_in,bytes_out,") {
                if session_index < 2 {
                    session_index += 1;
                }
                line.clear();
                continue;
            }

            if session_index < 2 {
                line.clear();
                continue;
            }

            let data: Vec<_> = line.split(',').collect();
            if data.len() < 3 {
                line.clear();
                continue;
            }

            let process: Vec<_> = data[0].split('.').collect();
            if process.len() < 2 {
                line.clear();
                continue;
            }

            let pid: Pid = process.last().unwrap().parse().unwrap();
            let bytes_in: u32 = data[1].parse().unwrap();
            let bytes_out: u32 = data[2].parse().unwrap();

            if let Err(_) = self.tx.send(ProcessCommandResult {
                pid,
                bytes_in,
                bytes_out,
                ..Default::default()
            }) {
                break;
            }

            line.clear();
        }

        let _ = child.kill();
    }
}

struct FrameRateRunner {
    tx: Sender<ProcessCommandResult>,
}

impl FrameRateRunner {
    fn new(tx: Sender<ProcessCommandResult>) -> Self {
        Self { tx }
    }

    fn run<T: IntoIterator<Item = Pid>>(self, pids: T) {
        let pids: Vec<_> = if csr_allow_unrestricted_dtrace() {
            pids.into_iter()
                .filter(|&pid| unsafe { !proc_is_translated(pid) })
                .collect()
        } else {
            pids.into_iter()
                .filter(|&pid| unsafe { !proc_is_translated(pid) })
                .filter(|&pid| match get_entitlements_for_pid(pid) {
                    Some(entitlements) => entitlements.get_task_allow,
                    None => true,
                })
                .collect()
        };

        if pids.is_empty() {
            return;
        }

        let mut command = Command::new("script");
        command.args(["-q", "/dev/null", "dtrace", "-Z", "-n"]);

        let mut methods = vec![];
        for pid in pids {
            methods.push(format!("objc{}:CAMetalLayer:-nextDrawable:entry", pid));
            methods.push(format!(
                "pid{}:QuartzCore:CA??Render??Surface??finalize():entry",
                pid
            ));
        }
        let mut methods = methods.join(",");
        methods.push_str("{trace(pid)}");

        command.arg(methods);

        let mut child = command
            .stdin(process::Stdio::piped())
            .stdout(process::Stdio::piped())
            .spawn()
            .unwrap();

        let mut buf = BufReader::new(child.stdout.as_mut().unwrap());
        let mut line = String::new();
        let mut session_index = 0;
        while let Ok(read) = buf.read_line(&mut line) {
            if read == 0 {
                break;
            }

            if line.starts_with("CPU") {
                if session_index < 1 {
                    session_index += 1;
                }
                line.clear();
                continue;
            }

            if session_index < 1 {
                line.clear();
                continue;
            }

            let data: Vec<_> = line.split_whitespace().collect();
            if data.len() < 4 {
                line.clear();
                continue;
            }

            let pid: Pid = data[3].parse().unwrap();

            if let Err(_) = self.tx.send(ProcessCommandResult {
                pid,
                frame: 1,
                ..Default::default()
            }) {
                break;
            }

            line.clear();
        }

        let _ = child.kill();
    }
}

static mut GET_PID_RESPONSIBLE: *mut c_void = ptr::null_mut();
static INIT: Once = Once::new();

pub fn get_pid_responsible() -> Option<extern "C" fn(libc::pid_t) -> libc::pid_t> {
    unsafe {
        INIT.call_once(|| {
            GET_PID_RESPONSIBLE = libc::dlsym(
                libc::RTLD_NEXT,
                "responsibility_get_pid_responsible_for_pid\0".as_ptr() as _,
            );
        });
        if GET_PID_RESPONSIBLE.is_null() {
            None
        } else {
            Some(std::mem::transmute(GET_PID_RESPONSIBLE))
        }
    }
}

#[derive(Deserialize)]
struct Entitlements {
    #[serde(rename = "com.apple.security.get-task-allow")]
    #[serde(default)]
    get_task_allow: bool,
}

fn get_entitlements_for_pid(pid: Pid) -> Option<Entitlements> {
    let mut command = Command::new("script");
    command.args([
        "-q",
        "/dev/null",
        "codesign",
        "--display",
        "--entitlements",
        "-",
        "--xml",
    ]);
    command.arg(format!("+{}", pid));

    let mut entitlements = Entitlements {
        get_task_allow: false,
    };

    let child = match command.output() {
        Ok(child) => child,
        Err(_) => return Some(entitlements),
    };

    let mut buf = Cursor::new(child.stdout);
    let mut line = String::new();

    match buf.read_line(&mut line) {
        Ok(n) if n > 0 => {
            if line.contains("code object is not signed at all") {
                return None;
            }
        }
        _ => return Some(entitlements),
    }

    line.clear();

    match buf.read_line(&mut line) {
        Ok(n) if n > 0 => {
            if let Ok(e) = plist::from_bytes(line.as_bytes()) {
                entitlements = e;
            }
        }
        _ => {}
    }
    Some(entitlements)
}

extern "C" {
    fn csr_get_active_config(config: *mut u32) -> i32;
}

const CSR_ALLOW_UNRESTRICTED_DTRACE: u32 = 1 << 5;

fn csr_allow_unrestricted_dtrace() -> bool {
    let mut config = 0;
    unsafe {
        let r = csr_get_active_config(&mut config);
        if r != 0 {
            return false;
        }
    }
    config & CSR_ALLOW_UNRESTRICTED_DTRACE == CSR_ALLOW_UNRESTRICTED_DTRACE
}

const PROC_PIDLISTFDS: libc::c_int = 1;

#[repr(C)]
struct proc_fd_info {
    pub proc_fd: i32,
    pub proc_fdtype: u32,
}

pub fn proc_fds(pid: Pid) -> Option<u32> {
    let mut buf: Vec<u8> = Vec::with_capacity(64 * mem::size_of::<proc_fd_info>());

    loop {
        let mut actual_buf_size = unsafe {
            libc::proc_pidinfo(
                pid as _,
                PROC_PIDLISTFDS,
                0,
                buf.as_mut_ptr() as _,
                buf.capacity() as _,
            )
        };
        if actual_buf_size < 0 {
            return None;
        }

        if actual_buf_size as usize >= buf.capacity() {
            buf.reserve(buf.capacity() * 2);
            continue;
        }

        break Some(actual_buf_size as u32 / mem::size_of::<proc_fd_info>() as u32);
    }
}

// https://opensource.apple.com/source/dtrace/dtrace-370.40.1/lib/libproc/libproc.c.auto.html
unsafe fn proc_is_translated(pid: Pid) -> bool {
    let mut mib = [
        libc::CTL_KERN,
        libc::KERN_PROC,
        libc::KERN_PROC_PID,
        pid as libc::c_int,
    ];
    let mut info: sysctl::kinfo_proc = MaybeUninit::uninit().assume_init();
    let mut size = mem::size_of::<sysctl::kinfo_proc>();
    let r = libc::sysctl(
        mib.as_mut_ptr(),
        mib.len() as _,
        &mut info as *mut _ as *mut c_void,
        &mut size,
        ptr::null_mut(),
        0,
    );

    if r == 0 && size >= mem::size_of::<sysctl::kinfo_proc>() {
        info.kp_proc.p_flag as u32 & sysctl::P_TRANSLATED == sysctl::P_TRANSLATED
    } else {
        false
    }
}
