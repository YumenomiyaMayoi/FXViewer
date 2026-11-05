//! Hardware sensors that `sysinfo` does not expose on Windows.
//!
//! Two independent sources, both optional and both silent when missing:
//!
//! * The PDH counter set `GPU Adapter Memory`. Task Manager draws its "shared
//!   GPU memory" bar from exactly these counters, and they are the only way to
//!   see the memory an integrated GPU borrows from system RAM — `sysinfo`
//!   reports dedicated VRAM only, which on an iGPU is a fixed 128 MB slice
//!   that never moves.
//! * NVML, for GPU temperature and board power. Loaded at runtime, so a
//!   machine without an NVIDIA driver (or without the DLL) simply gets
//!   `None` instead of a startup failure.
//!
//! Nothing here is allowed to panic or to kill the sampler: every failure path
//! degrades to "this sensor is not available", which the UI already handles.

#[derive(Default, Clone, Copy, Debug)]
pub struct GpuReading {
    pub used: u64,
    pub total: u64,
    pub temp_c: Option<f32>,
    pub power_w: Option<f32>,
}

/// Commit charge and page-file occupancy, in bytes.
///
/// Windows has no swap counter. The page file backs committed pages, but the
/// kernel only writes to it under pressure, so the number that fills up first
/// is **committed** memory against the commit limit; what has actually landed
/// in `pagefile.sys` is a separate, much smaller figure.
#[derive(Default, Clone, Copy, Debug)]
pub struct MemReading {
    pub committed: u64,
    pub commit_limit: u64,
    pub pagefile_used: u64,
}

/// Identity of the physical device behind a mounted volume.
///
/// Windows gives a drive letter, but "C:" says nothing about what is actually
/// plugged in — the number Task Manager shows, the product string the device
/// reports, and the bus it sits on do.
#[derive(Default, Clone, Debug)]
pub struct VolumeInfo {
    /// Task Manager's "Disk N".
    pub disk_no: Option<u32>,
    /// The device's own product string, e.g. "SDHC Card".
    pub product: Option<String>,
    /// Bus class, already collapsed to something short: "NVMe", "SATA", "USB".
    pub bus: Option<&'static str>,
}

#[cfg(windows)]
mod win {
    use std::ffi::CStr;

    use windows_sys::Win32::Foundation::{
        CloseHandle, FreeLibrary, FARPROC, HANDLE, HMODULE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Ioctl::{
        IOCTL_STORAGE_GET_DEVICE_NUMBER, IOCTL_STORAGE_QUERY_PROPERTY,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows_sys::Win32::System::Performance::{
        PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
        PdhGetFormattedCounterValue, PdhOpenQueryW, PDH_FMT_COUNTERVALUE,
        PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_FMT_LARGE, PDH_HCOUNTER, PDH_HQUERY,
        PDH_MORE_DATA,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETWORKAREA};

    use super::{GpuReading, MemReading, VolumeInfo};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Usable desktop area of the primary monitor — the screen minus the
    /// taskbar — as `(x, y, width, height)` in points.
    ///
    /// A window larger than this cannot be shown whole: Windows clamps it to
    /// the *screen*, not the work area, so whatever sits at the bottom (here:
    /// the status bar) ends up under the taskbar. The app asks once at startup
    /// so it opens at a size and position that actually fit.
    pub fn work_area_points(ppp: f32) -> Option<(f32, f32, f32, f32)> {
        let mut r = windows_sys::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        let ok = unsafe {
            SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                &mut r as *mut _ as *mut core::ffi::c_void,
                0,
            )
        };
        if ok == 0 {
            return None;
        }
        let ppp = if ppp > 0.0 { ppp } else { 1.0 };
        Some((
            r.left as f32 / ppp,
            r.top as f32 / ppp,
            (r.right - r.left) as f32 / ppp,
            (r.bottom - r.top) as f32 / ppp,
        ))
    }

    // ---------------------------------------------------------------- PDH ---

    struct MemProbe {
        query: PDH_HQUERY,
        shared: PDH_HCOUNTER,
    }

    impl MemProbe {
        fn new() -> Option<Self> {
            unsafe {
                let mut query: PDH_HQUERY = std::ptr::null_mut();
                if PdhOpenQueryW(std::ptr::null(), 0, &mut query) != 0 {
                    return None;
                }
                let mut shared: PDH_HCOUNTER = std::ptr::null_mut();
                let path = wide("\\GPU Adapter Memory(*)\\Shared Usage");
                if PdhAddEnglishCounterW(query, path.as_ptr(), 0, &mut shared) != 0 {
                    PdhCloseQuery(query);
                    return None;
                }
                Some(Self { query, shared })
            }
        }

        fn read(&mut self) -> u64 {
            unsafe {
                // The first collect only primes the query; until it has run
                // twice the array returns no data, which reads as zero.
                if PdhCollectQueryData(self.query) != 0 {
                    return 0;
                }
                sum_array(self.shared)
            }
        }
    }

    impl Drop for MemProbe {
        fn drop(&mut self) {
            unsafe {
                PdhCloseQuery(self.query);
            }
        }
    }

    /// Commit charge plus real page-file occupancy.
    ///
    /// `\Paging File(_Total)\% Usage` is the same counter Task Manager's
    /// page-file figure comes from; multiplying it by the configured page file
    /// size gives bytes on disk. Three counters share one query, so the whole
    /// reading is one `PdhCollectQueryData`.
    struct CommitProbe {
        query: PDH_HQUERY,
        committed: PDH_HCOUNTER,
        limit: PDH_HCOUNTER,
        pagefile: PDH_HCOUNTER,
    }

    impl CommitProbe {
        fn new() -> Option<Self> {
            unsafe {
                let mut query: PDH_HQUERY = std::ptr::null_mut();
                if PdhOpenQueryW(std::ptr::null(), 0, &mut query) != 0 {
                    return None;
                }
                let mut committed: PDH_HCOUNTER = std::ptr::null_mut();
                let mut limit: PDH_HCOUNTER = std::ptr::null_mut();
                let mut pagefile: PDH_HCOUNTER = std::ptr::null_mut();
                let ok = PdhAddEnglishCounterW(
                    query,
                    wide("\\Memory\\Committed Bytes").as_ptr(),
                    0,
                    &mut committed,
                ) == 0
                    && PdhAddEnglishCounterW(
                        query,
                        wide("\\Memory\\Commit Limit").as_ptr(),
                        0,
                        &mut limit,
                    ) == 0
                    && PdhAddEnglishCounterW(
                        query,
                        wide("\\Paging File(_Total)\\% Usage").as_ptr(),
                        0,
                        &mut pagefile,
                    ) == 0;
                if !ok {
                    PdhCloseQuery(query);
                    return None;
                }
                Some(Self {
                    query,
                    committed,
                    limit,
                    pagefile,
                })
            }
        }

        fn read(&mut self, pagefile_total: u64) -> Option<MemReading> {
            unsafe {
                if PdhCollectQueryData(self.query) != 0 {
                    return None;
                }
                let committed = single(self.committed)?;
                let limit = single(self.limit)?;
                let pct = single(self.pagefile).unwrap_or(0.0).clamp(0.0, 100.0);
                Some(MemReading {
                    committed: committed.max(0.0) as u64,
                    commit_limit: limit.max(0.0) as u64,
                    pagefile_used: (pagefile_total as f64 * pct / 100.0).round() as u64,
                })
            }
        }
    }

    impl Drop for CommitProbe {
        fn drop(&mut self) {
            unsafe {
                PdhCloseQuery(self.query);
            }
        }
    }

    /// One non-wildcard counter, as a double. Bytes go through `f64` too: a
    /// commit limit is under 2^53, so the conversion is exact.
    unsafe fn single(counter: PDH_HCOUNTER) -> Option<f64> {
        if counter.is_null() {
            return None;
        }
        let mut value = PDH_FMT_COUNTERVALUE::default();
        let mut kind: u32 = 0;
        if PdhGetFormattedCounterValue(counter, PDH_FMT_DOUBLE, &mut kind, &mut value) != 0 {
            return None;
        }
        // 0 = PDH_CSTATUS_VALID_DATA, 1 = PDH_CSTATUS_NEW_DATA. Anything else
        // means the counter set vanished (sleep/resume, driver reload).
        if value.CStatus > 1 {
            return None;
        }
        let v = value.Anonymous.doubleValue;
        v.is_finite().then_some(v)
    }

    /// Sums one wildcard counter across every adapter instance.
    ///
    /// `PdhGetFormattedCounterArrayW` reports the required buffer size in
    /// bytes, and the buffer holds structures containing pointers, so it is
    /// allocated as `usize` words rather than bytes: `Vec<u8>` only guarantees
    /// byte alignment.
    unsafe fn sum_array(counter: PDH_HCOUNTER) -> u64 {
        if counter.is_null() {
            return 0;
        }
        let mut size: u32 = 0;
        let mut count: u32 = 0;
        let rc = PdhGetFormattedCounterArrayW(
            counter,
            PDH_FMT_LARGE,
            &mut size,
            &mut count,
            std::ptr::null_mut(),
        );
        if rc != PDH_MORE_DATA || size == 0 {
            return 0;
        }
        let words = (size as usize).div_ceil(std::mem::size_of::<usize>());
        let mut buf: Vec<usize> = vec![0; words];
        let rc = PdhGetFormattedCounterArrayW(
            counter,
            PDH_FMT_LARGE,
            &mut size,
            &mut count,
            buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W,
        );
        if rc != 0 {
            return 0;
        }
        let items = std::slice::from_raw_parts(
            buf.as_ptr() as *const PDH_FMT_COUNTERVALUE_ITEM_W,
            count as usize,
        );
        let mut sum: u64 = 0;
        for it in items {
            let v = it.FmtValue.Anonymous.largeValue;
            if v > 0 {
                sum = sum.saturating_add(v as u64);
            }
        }
        sum
    }

    // --------------------------------------------------------------- NVML ---

    const NVML_SUCCESS: i32 = 0;
    /// `NVML_TEMPERATURE_GPU`: the die temperature, not a board sensor.
    const NVML_TEMPERATURE_GPU: u32 = 0;

    #[repr(C)]
    #[derive(Default)]
    struct NvmlMemory {
        total: u64,
        free: u64,
        used: u64,
    }

    type Device = *mut core::ffi::c_void;

    struct Nvml {
        lib: HMODULE,
        devices: Vec<(String, Device)>,
        temperature: unsafe extern "system" fn(Device, u32, *mut u32) -> i32,
        power: unsafe extern "system" fn(Device, *mut u32) -> i32,
        memory: unsafe extern "system" fn(Device, *mut NvmlMemory) -> i32,
    }

    unsafe fn symbol<T: Copy>(lib: HMODULE, name: &[u8]) -> Option<T> {
        debug_assert_eq!(name.last(), Some(&0));
        let p: FARPROC = GetProcAddress(lib, name.as_ptr());
        let f = p?;
        Some(std::mem::transmute_copy(&f))
    }

    impl Nvml {
        unsafe fn load() -> Option<Self> {
            let mut lib = LoadLibraryW(wide("nvml.dll").as_ptr());
            if lib.is_null() {
                // Older drivers keep it beside the SMI tool instead of in the
                // system directory.
                lib = LoadLibraryW(
                    wide("C:\\Program Files\\NVIDIA Corporation\\NVSMI\\nvml.dll").as_ptr(),
                );
            }
            if lib.is_null() {
                return None;
            }

            let init: unsafe extern "system" fn() -> i32 = match symbol(lib, b"nvmlInit_v2\0") {
                Some(f) => f,
                None => {
                    FreeLibrary(lib);
                    return None;
                }
            };
            if init() != NVML_SUCCESS {
                FreeLibrary(lib);
                return None;
            }

            let count_fn: unsafe extern "system" fn(*mut u32) -> i32 =
                match symbol(lib, b"nvmlDeviceGetCount_v2\0") {
                    Some(f) => f,
                    None => {
                        FreeLibrary(lib);
                        return None;
                    }
                };
            let handle_fn: unsafe extern "system" fn(u32, *mut Device) -> i32 =
                match symbol(lib, b"nvmlDeviceGetHandleByIndex_v2\0") {
                    Some(f) => f,
                    None => {
                        FreeLibrary(lib);
                        return None;
                    }
                };
            let name_fn: unsafe extern "system" fn(Device, *mut i8, u32) -> i32 =
                match symbol(lib, b"nvmlDeviceGetName\0") {
                    Some(f) => f,
                    None => {
                        FreeLibrary(lib);
                        return None;
                    }
                };

            let mut devices = Vec::new();
            let mut count: u32 = 0;
            if count_fn(&mut count) == NVML_SUCCESS {
                for i in 0..count {
                    let mut dev: Device = std::ptr::null_mut();
                    if handle_fn(i, &mut dev) != NVML_SUCCESS || dev.is_null() {
                        continue;
                    }
                    let mut buf = [0i8; 96];
                    if name_fn(dev, buf.as_mut_ptr(), buf.len() as u32) != NVML_SUCCESS {
                        continue;
                    }
                    let name = CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned();
                    devices.push((name, dev));
                }
            }

            // The three reading functions arrived with different driver
            // generations; each is optional on its own.
            let temperature = symbol(lib, b"nvmlDeviceGetTemperature\0");
            let power = symbol(lib, b"nvmlDeviceGetPowerUsage\0");
            let memory = symbol(lib, b"nvmlDeviceGetMemoryInfo\0");
            let (temperature, power, memory) = match (temperature, power, memory) {
                (Some(t), Some(p), Some(m)) => (t, p, m),
                _ => {
                    FreeLibrary(lib);
                    return None;
                }
            };

            Some(Self {
                lib,
                devices,
                temperature,
                power,
                memory,
            })
        }

        fn device_for(&self, model: &str) -> Option<Device> {
            let want = model.trim().to_ascii_lowercase();
            if want.is_empty() {
                return None;
            }
            let exact = self
                .devices
                .iter()
                .find(|(n, _)| n.to_ascii_lowercase() == want);
            let loose = self.devices.iter().find(|(n, _)| {
                let n = n.to_ascii_lowercase();
                n.contains(&want) || want.contains(&n)
            });
            exact.or(loose).map(|(_, d)| *d)
        }

        fn read(&self, model: &str) -> Option<GpuReading> {
            let dev = self.device_for(model)?;
            let mut out = GpuReading::default();
            let mut any = false;

            unsafe {
                let mut mem = NvmlMemory::default();
                if (self.memory)(dev, &mut mem) == NVML_SUCCESS {
                    out.used = mem.used;
                    out.total = mem.total;
                    any = true;
                }
                let mut t: u32 = 0;
                if (self.temperature)(dev, NVML_TEMPERATURE_GPU, &mut t) == NVML_SUCCESS
                    && t > 0
                    && t < 150
                {
                    out.temp_c = Some(t as f32);
                    any = true;
                }
                let mut mw: u32 = 0;
                if (self.power)(dev, &mut mw) == NVML_SUCCESS && mw > 0 {
                    out.power_w = Some(mw as f32 / 1000.0);
                    any = true;
                }
            }

            if any {
                Some(out)
            } else {
                None
            }
        }
    }

    impl Drop for Nvml {
        fn drop(&mut self) {
            unsafe {
                FreeLibrary(self.lib);
            }
        }
    }

    // ------------------------------------------------------------- volumes ---

    /// Reads the physical disk number and the device descriptor for a volume.
    ///
    /// Both ioctls are read-only and need no elevation: a handle opened with
    /// zero access is enough. Any failure — no permission, a network share, a
    /// mount point without a drive letter — means "no extra identity", which
    /// the caller renders as the letter alone.
    pub fn volume_info(mount: &str) -> Option<VolumeInfo> {
        let letter = mount.trim_end_matches(['\\', '/']);
        let bytes = letter.as_bytes();
        if bytes.len() != 2 || bytes[1] != b':' || !bytes[0].is_ascii_alphabetic() {
            return None;
        }
        let path = wide(&format!("\\\\.\\{}", letter));

        unsafe {
            let h: HANDLE = CreateFileW(
                path.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            );
            if h == INVALID_HANDLE_VALUE {
                return None;
            }

            let mut info = VolumeInfo::default();
            let mut returned: u32 = 0;

            // STORAGE_DEVICE_NUMBER: { DeviceType, DeviceNumber, PartitionNumber }
            let mut sdn = [0u8; 12];
            if DeviceIoControl(
                h,
                IOCTL_STORAGE_GET_DEVICE_NUMBER,
                std::ptr::null(),
                0,
                sdn.as_mut_ptr() as *mut _,
                sdn.len() as u32,
                &mut returned,
                std::ptr::null_mut(),
            ) != 0
            {
                info.disk_no = Some(u32::from_ne_bytes([sdn[4], sdn[5], sdn[6], sdn[7]]));
            }

            // STORAGE_PROPERTY_QUERY { StorageDeviceProperty, StandardQuery }
            #[repr(C)]
            struct Query {
                property_id: u32,
                query_type: u32,
                extra: [u8; 8],
            }
            let query = Query {
                property_id: 0,
                query_type: 0,
                extra: [0; 8],
            };
            let mut buf = [0u8; 1024];
            if DeviceIoControl(
                h,
                IOCTL_STORAGE_QUERY_PROPERTY,
                &query as *const Query as *const _,
                std::mem::size_of::<Query>() as u32,
                buf.as_mut_ptr() as *mut _,
                buf.len() as u32,
                &mut returned,
                std::ptr::null_mut(),
            ) != 0
            {
                // STORAGE_DEVICE_DESCRIPTOR layout, stable since Vista:
                //  0 Version, 4 Size, 8 DeviceType, 9 Modifier, 10 Removable,
                //  11 Queueing, 12 VendorIdOffset, 16 ProductIdOffset,
                //  20 RevisionOffset, 24 SerialOffset, 28 BusType,
                //  32 RawPropertiesLength, 36 RawDeviceProperties.
                let word = |off: usize| {
                    u32::from_ne_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]) as usize
                };
                info.bus = Some(bus_label(word(28) as u32));
                let at = word(16);
                if at > 0 && at < buf.len() {
                    info.product = ascii_at(&buf, at).filter(|p| looks_like_product(p));
                }
            }

            CloseHandle(h);
            if info.disk_no.is_none() && info.product.is_none() {
                None
            } else {
                Some(info)
            }
        }
    }

    /// NUL-terminated ASCII at a fixed offset in the descriptor blob.
    fn ascii_at(buf: &[u8], at: usize) -> Option<String> {
        let tail = &buf[at..];
        let end = tail.iter().position(|b| *b == 0).unwrap_or(tail.len());
        let s: String = tail[..end]
            .iter()
            .map(|b| {
                if b.is_ascii_graphic() || *b == b' ' {
                    *b as char
                } else {
                    ' '
                }
            })
            .collect();
        let s = s.trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }

    /// Some drivers report a serial number where the product string belongs,
    /// and those are all hex digits and separators ("FFFF_FFFF_FFFF_FFFF.").
    /// A real model name always contains actual letters.
    fn looks_like_product(s: &str) -> bool {
        let letters = s.chars().filter(|c| c.is_ascii_alphabetic()).count();
        let hexish = s
            .chars()
            .all(|c| c.is_ascii_hexdigit() || matches!(c, '_' | '-' | '.' | ' '));
        letters >= 3 && !hexish
    }

    fn bus_label(bus: u32) -> &'static str {
        match bus {
            7 => "USB",
            11 => "SATA",
            12 => "SD",
            13 => "eMMC",
            17 => "NVMe",
            1..=3 => "SCSI",
            14..=16 => "VIRTUAL",
            _ => "DISK",
        }
    }

    pub struct Sensors {
        mem: Option<MemProbe>,
        commit: Option<CommitProbe>,
        nvml: Option<Nvml>,
    }

    impl Sensors {
        pub fn new() -> Self {
            Self {
                mem: MemProbe::new(),
                commit: CommitProbe::new(),
                nvml: unsafe { Nvml::load() },
            }
        }

        /// Machine-wide shared GPU memory in bytes, summed over every adapter
        /// instance. Zero when the counter set is unavailable.
        pub fn shared_memory(&mut self) -> u64 {
            match &mut self.mem {
                Some(p) => p.read(),
                None => 0,
            }
        }

        /// Commit charge and page-file occupancy. `pagefile_total` is the
        /// configured page file size in bytes; `None` when PDH is unavailable.
        pub fn memory(&mut self, pagefile_total: u64) -> Option<MemReading> {
            self.commit.as_mut()?.read(pagefile_total)
        }

        /// Per-adapter reading, matched by model name. `None` when there is no
        /// NVML device for this GPU.
        pub fn read_gpu(&self, model: &str) -> Option<GpuReading> {
            self.nvml.as_ref()?.read(model)
        }
    }
}

#[cfg(not(windows))]
mod win {
    use super::{GpuReading, MemReading, VolumeInfo};

    /// No work-area query outside Windows: the caller keeps the design size.
    pub fn work_area_points(_ppp: f32) -> Option<(f32, f32, f32, f32)> {
        None
    }

    pub struct Sensors;

    impl Sensors {
        pub fn new() -> Self {
            Self
        }
        pub fn shared_memory(&mut self) -> u64 {
            0
        }
        pub fn read_gpu(&self, _model: &str) -> Option<GpuReading> {
            None
        }
    }
}

pub use win::{volume_info, work_area_points, Sensors};
