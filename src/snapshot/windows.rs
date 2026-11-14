use std::io;
use std::mem::size_of;

use windows_sys::Wdk::System::SystemInformation::{
    NtQuerySystemInformation, SystemProcessInformation,
};
use windows_sys::Win32::Foundation::{FILETIME, HANDLE, STATUS_INFO_LENGTH_MISMATCH};
use windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows_sys::Win32::System::Threading::{ALL_PROCESSOR_GROUPS, GetActiveProcessorCount};

use super::{Proc, Snapshot};

/// Layout of SYSTEM_PROCESS_INFORMATION through WorkingSetSize, as ntdll
/// fills it on every Windows release since Vista; the SDK hides most fields.
#[repr(C)]
struct ProcessInformation {
    next_entry_offset: u32,
    number_of_threads: u32,
    working_set_private_size: i64,
    hard_fault_count: u32,
    number_of_threads_high_watermark: u32,
    cycle_time: u64,
    create_time: i64,
    user_time: i64,
    kernel_time: i64,
    image_name_length: u16,
    image_name_maximum_length: u16,
    image_name_buffer: *const u16,
    base_priority: i32,
    unique_process_id: HANDLE,
    inherited_from_unique_process_id: HANDLE,
    handle_count: u32,
    session_id: u32,
    unique_process_key: usize,
    peak_virtual_size: usize,
    virtual_size: usize,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
}

/// Reads every process in one NtQuerySystemInformation call, reusing its buffer.
pub struct Lister {
    // u64 elements keep the records 8-byte aligned.
    buf: Vec<u64>,
}

impl Lister {
    /// A lister with room for a typical desktop's process table.
    pub fn new() -> Self {
        Self {
            buf: vec![0; (1 << 18) / 8],
        }
    }

    /// Every process on the host right now.
    pub fn take(&mut self) -> io::Result<Snapshot> {
        let now_ns = system_time_ns();
        self.query()?;
        Ok(Snapshot {
            procs: self.records().filter_map(to_proc).collect(),
            now_ns,
            resolution_ns: 100,
        })
    }

    /// Fills the buffer with the process table, growing it until the table fits.
    fn query(&mut self) -> io::Result<()> {
        loop {
            let bytes = u32::try_from(self.buf.len() * 8)
                .map_err(|_| io::Error::other("process table too large"))?;
            let mut needed = 0u32;
            // SAFETY: the buffer holds `bytes` writable bytes and `needed` is a valid out-parameter.
            let status = unsafe {
                NtQuerySystemInformation(
                    SystemProcessInformation,
                    self.buf.as_mut_ptr().cast(),
                    bytes,
                    &raw mut needed,
                )
            };
            if status == STATUS_INFO_LENGTH_MISMATCH {
                // Room for processes started before the next query.
                self.buf.resize((needed as usize + (1 << 16)) / 8 + 1, 0);
                continue;
            }
            if status < 0 {
                return Err(io::Error::other(format!(
                    "NtQuerySystemInformation failed: {status:#x}"
                )));
            }
            return Ok(());
        }
    }

    /// The records of the table the last query filled, each linked to the next by its offset.
    fn records(&self) -> impl Iterator<Item = &ProcessInformation> {
        let base = self.buf.as_ptr().cast::<u8>();
        let total = self.buf.len() * 8;
        let mut offset = Some(0usize);
        std::iter::from_fn(move || {
            let at = offset.filter(|&at| at + size_of::<ProcessInformation>() <= total)?;
            // SAFETY: ntdll lays records at 8-aligned offsets inside the filled buffer.
            let info = unsafe { &*base.add(at).cast::<ProcessInformation>() };
            offset = (info.next_entry_offset != 0).then(|| at + info.next_entry_offset as usize);
            Some(info)
        })
    }
}

/// The process a record describes, or `None` for the System Idle Process
/// (pid 0), whose CPU time is idle time.
fn to_proc(info: &ProcessInformation) -> Option<Proc> {
    let pid = info.unique_process_id as usize as i64;
    (pid != 0).then(|| Proc {
        pid,
        ppid: info.inherited_from_unique_process_id as usize as i64,
        pgid: 0,
        start_ns: info.create_time * 100,
        cpu_ns: (info.user_time + info.kernel_time) * 100,
        child_cpu_ns: 0,
        mem_bytes: info.working_set_private_size,
        name: image_name(info),
    })
}

/// The wall clock in nanoseconds since 1601, the epoch process creation times count from.
fn system_time_ns() -> i64 {
    let mut now = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    // SAFETY: `now` is a valid FILETIME the call writes into.
    unsafe { GetSystemTimeAsFileTime(&raw mut now) };
    ((u64::from(now.dwHighDateTime) << 32 | u64::from(now.dwLowDateTime)) as i64) * 100
}

/// Logical CPUs online across every processor group.
pub fn cpu_count() -> i64 {
    // SAFETY: GetActiveProcessorCount has no preconditions.
    i64::from(unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) }).max(1)
}

/// The program a record names: its image name without the `.exe`.
fn image_name(info: &ProcessInformation) -> String {
    let units = usize::from(info.image_name_length) / 2;
    if units == 0 || info.image_name_buffer.is_null() {
        return "System".to_owned();
    }
    // SAFETY: ntdll points the name at `image_name_length` bytes of UTF-16 inside the buffer.
    let wide = unsafe { std::slice::from_raw_parts(info.image_name_buffer, units) };
    program_name(String::from_utf16_lossy(wide))
}

/// Windows image names carry their extension; the program is the stem.
fn program_name(image: String) -> String {
    match image.len().checked_sub(4) {
        Some(stem)
            if image.is_char_boundary(stem) && image[stem..].eq_ignore_ascii_case(".exe") =>
        {
            image[..stem].to_owned()
        }
        _ => image,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_names_drop_the_exe_extension_in_any_case() {
        assert_eq!(program_name("firefox.exe".to_owned()), "firefox");
        assert_eq!(program_name("CMD.EXE".to_owned()), "CMD");
        assert_eq!(program_name("Registry".to_owned()), "Registry");
        assert_eq!(program_name("ü.exe".to_owned()), "ü");
        assert_eq!(program_name("exe".to_owned()), "exe");
    }
}
