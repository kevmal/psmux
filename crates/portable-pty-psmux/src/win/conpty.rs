use crate::cmdbuilder::CommandBuilder;
use crate::win::psuedocon::PsuedoCon;
use crate::{Child, MasterPty, PtyPair, PtySize, PtySystem, SlavePty};
use anyhow::Error;
use filedescriptor::FileDescriptor;
use std::sync::{Arc, Mutex};
use winapi::um::wincon::COORD;

/// Create a pipe pair with an explicit buffer size.
///
/// Windows Terminal uses 128 KB pipe buffers for ConPTY I/O.  The default
/// `CreatePipe(..., 0)` typically gets 4 KB, which forces more frequent
/// kernel transitions during high-throughput output (e.g. `cat large_file`).
/// Using 64 KB matches Windows Terminal's approach and reduces syscall
/// overhead for both input (mouse/keyboard) and output.
fn create_pipe_with_buffer(size: u32) -> anyhow::Result<(FileDescriptor, FileDescriptor)> {
    use std::os::windows::io::FromRawHandle;
    use std::ptr;
    use winapi::shared::minwindef::FALSE;
    use winapi::um::handleapi::INVALID_HANDLE_VALUE;
    use winapi::um::minwinbase::SECURITY_ATTRIBUTES;
    use winapi::um::namedpipeapi::CreatePipe;
    use winapi::um::winnt::HANDLE;

    // Non-inheritable. ConPTY hands stdio to the child via the
    // PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE attribute (with bInheritHandles=FALSE
    // in spawn_command), and CreatePseudoConsole duplicates these handles
    // internally, so they never need to be inheritable. Marking them inheritable
    // only lets concurrent bInheritHandles=TRUE helper spawns in the server
    // process (run-shell, #() format, if-shell, clipboard pipes) inherit a
    // duplicate of a pane's conin handle, breaking the child shell's sole
    // ownership of console input and crashing it with "The handle is invalid".
    let mut sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: FALSE as _,
    };
    let mut read: HANDLE = INVALID_HANDLE_VALUE;
    let mut write: HANDLE = INVALID_HANDLE_VALUE;
    if unsafe { CreatePipe(&mut read, &mut write, &mut sa, size) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe {(
        FileDescriptor::from_raw_handle(read as _),
        FileDescriptor::from_raw_handle(write as _),
    )})
}

#[derive(Default)]
pub struct ConPtySystem {}

impl PtySystem for ConPtySystem {
    fn openpty(&self, size: PtySize) -> anyhow::Result<PtyPair> {
        // Use 64KB pipe buffers (Windows Terminal uses 128KB).
        // Default CreatePipe(..., 0) = ~4KB, causing frequent kernel round-trips.
        const PIPE_BUF: u32 = 64 * 1024;
        let (stdin_read, stdin_write) = create_pipe_with_buffer(PIPE_BUF)?;
        let (stdout_read, stdout_write) = create_pipe_with_buffer(PIPE_BUF)?;
        let input_probe = stdin_write.try_clone().ok();

        let con = PsuedoCon::new(
            COORD {
                X: size.cols as i16,
                Y: size.rows as i16,
            },
            stdin_read,
            stdout_write,
        )?;

        let master = ConPtyMasterPty {
            inner: Arc::new(Mutex::new(Inner {
                con,
                readable: stdout_read,
                writable: Some(stdin_write),
                input_probe,
                size,
            })),
        };

        let slave = ConPtySlavePty {
            inner: master.inner.clone(),
        };

        Ok(PtyPair {
            master: Box::new(master),
            slave: Box::new(slave),
        })
    }
}

struct Inner {
    con: PsuedoCon,
    readable: FileDescriptor,
    writable: Option<FileDescriptor>,
    /// Our own handle on the write end of the input pipe, kept so
    /// `try_clone_input_pending` can ask the pipe how many written bytes
    /// conhost has not read yet.  Never written to.  It must be the write
    /// end: a duplicate of the read end shares conhost's file object, and a
    /// query on it waits behind conhost's blocking ReadFile until we write
    /// something, which is a deadlock for a writer waiting for the pipe to
    /// drain.
    input_probe: Option<FileDescriptor>,
    size: PtySize,
}

impl Inner {
    pub fn resize(
        &mut self,
        num_rows: u16,
        num_cols: u16,
        pixel_width: u16,
        pixel_height: u16,
    ) -> Result<(), Error> {
        self.con.resize(COORD {
            X: num_cols as i16,
            Y: num_rows as i16,
        })?;
        self.size = PtySize {
            rows: num_rows,
            cols: num_cols,
            pixel_width,
            pixel_height,
        };
        Ok(())
    }
}

#[derive(Clone)]
pub struct ConPtyMasterPty {
    inner: Arc<Mutex<Inner>>,
}

pub struct ConPtySlavePty {
    inner: Arc<Mutex<Inner>>,
}

impl MasterPty for ConPtyMasterPty {
    fn resize(&self, size: PtySize) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.resize(size.rows, size.cols, size.pixel_width, size.pixel_height)
    }

    fn get_size(&self) -> Result<PtySize, Error> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.size)
    }

    fn try_clone_reader(&self) -> anyhow::Result<Box<dyn std::io::Read + Send>> {
        Ok(Box::new(self.inner.lock().unwrap().readable.try_clone()?))
    }

    fn take_writer(&self) -> anyhow::Result<Box<dyn std::io::Write + Send>> {
        Ok(Box::new(
            self.inner
                .lock()
                .unwrap()
                .writable
                .take()
                .ok_or_else(|| anyhow::anyhow!("writer already taken"))?,
        ))
    }

    fn conpty_passthrough_mode(&self) -> Option<bool> {
        Some(self.inner.lock().unwrap().con.used_passthrough)
    }

    fn try_clone_input_pending(&self) -> Option<Box<dyn Fn() -> std::io::Result<usize> + Send>> {
        let fd = self.inner.lock().unwrap().input_probe.as_ref()?.try_clone().ok()?;
        Some(Box::new(move || pipe_pending_bytes(&fd)))
    }
}

impl SlavePty for ConPtySlavePty {
    fn spawn_command(&self, cmd: CommandBuilder) -> anyhow::Result<Box<dyn Child + Send + Sync>> {
        let mut inner = self.inner.lock().unwrap();
        match inner.con.spawn_command(cmd.clone()) {
            Ok(child) => Ok(Box::new(child)),
            Err(e) if inner.con.used_passthrough && is_invalid_parameter(&e) => {
                // CreateProcessW rejected the ConPTY handle that was created
                // with PSEUDOCONSOLE_PASSTHROUGH_MODE.  Some Windows 11 builds
                // (notably Insider/Canary builds like 26200) accept the flag
                // during CreatePseudoConsole but later fail in CreateProcessW
                // with ERROR_INVALID_PARAMETER (87).
                //
                // Recovery: recreate the ConPTY without passthrough mode and
                // create fresh pipe pairs for the new pseudo-console.
                log::warn!(
                    "CreateProcessW failed with ERROR_INVALID_PARAMETER while using \
                     ConPTY passthrough mode; retrying without passthrough"
                );
                const PIPE_BUF: u32 = 64 * 1024;
                let (stdin_read, stdin_write) = create_pipe_with_buffer(PIPE_BUF)?;
                let (stdout_read, stdout_write) = create_pipe_with_buffer(PIPE_BUF)?;
                let input_probe = stdin_write.try_clone().ok();

                let new_con = PsuedoCon::new_without_passthrough(
                    COORD {
                        X: inner.size.cols as i16,
                        Y: inner.size.rows as i16,
                    },
                    stdin_read,
                    stdout_write,
                )?;

                // Replace the ConPTY and pipe endpoints inside Inner.
                // At this point nobody has cloned the reader or taken the
                // writer yet (pane.rs acquires them after spawn_command),
                // so the old FileDescriptors are dropped cleanly.
                inner.con = new_con;
                inner.readable = stdout_read;
                inner.writable = Some(stdin_write);
                inner.input_probe = input_probe;

                let child = inner.con.spawn_command(cmd)?;
                Ok(Box::new(child))
            }
            Err(e) => Err(e),
        }
    }
}

/// Bytes written to the pipe that the read side has not consumed yet, asked
/// of the write end through `FilePipeLocalInformation`: the unread bytes are
/// the inbound quota minus the write quota still available.  The write end
/// is its own file object, so this never waits on the read side's ReadFile
/// (see `Inner::input_probe`).
fn pipe_pending_bytes(fd: &FileDescriptor) -> std::io::Result<usize> {
    use std::os::windows::io::AsRawHandle;

    #[repr(C)]
    struct IoStatusBlock {
        status: usize,
        information: usize,
    }

    #[repr(C)]
    #[derive(Default)]
    struct FilePipeLocalInformation {
        named_pipe_type: u32,
        named_pipe_configuration: u32,
        maximum_instances: u32,
        current_instances: u32,
        inbound_quota: u32,
        read_data_available: u32,
        outbound_quota: u32,
        write_quota_available: u32,
        named_pipe_state: u32,
        named_pipe_end: u32,
    }

    const FILE_PIPE_LOCAL_INFORMATION: u32 = 24;
    const FILE_PIPE_SERVER_END: u32 = 1;

    #[link(name = "ntdll")]
    extern "system" {
        fn NtQueryInformationFile(
            file_handle: *mut std::ffi::c_void,
            io_status_block: *mut IoStatusBlock,
            file_information: *mut std::ffi::c_void,
            length: u32,
            file_information_class: u32,
        ) -> i32;
    }

    let mut iosb = IoStatusBlock { status: 0, information: 0 };
    let mut info = FilePipeLocalInformation::default();
    let status = unsafe {
        NtQueryInformationFile(
            fd.as_raw_handle() as _,
            &mut iosb,
            &mut info as *mut _ as *mut std::ffi::c_void,
            std::mem::size_of::<FilePipeLocalInformation>() as u32,
            FILE_PIPE_LOCAL_INFORMATION,
        )
    };
    if status < 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("NtQueryInformationFile(FilePipeLocalInformation) failed: NTSTATUS {status:#x}"),
        ));
    }
    // CreatePipe hands out the write end as the pipe's client, whose writes
    // fill the inbound queue; a server-end write handle would fill outbound.
    let (quota, available) = if info.named_pipe_end == FILE_PIPE_SERVER_END {
        (info.outbound_quota, info.write_quota_available)
    } else {
        (info.inbound_quota, info.write_quota_available)
    };
    Ok(quota.saturating_sub(available) as usize)
}

/// Check if an error chain contains Windows ERROR_INVALID_PARAMETER (87).
/// The OS error number is locale-independent; the textual message varies
/// (e.g. "Falscher Parameter" in German).
fn is_invalid_parameter(e: &anyhow::Error) -> bool {
    let msg = format!("{}", e);
    msg.contains("os error 87")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    const CHILD_ENV: &str = "PORTABLE_PTY_CONPTY_PASSTHROUGH_TEST_CHILD";

    #[test]
    fn reports_when_passthrough_is_disabled_by_environment() {
        if std::env::var_os(CHILD_ENV).is_some() {
            let pair = ConPtySystem::default().openpty(PtySize::default()).unwrap();

            assert_eq!(pair.master.conpty_passthrough_mode(), Some(false));
            return;
        }

        let exe = std::env::current_exe().unwrap();
        let output = Command::new(exe)
            .args([
                "--exact",
                "win::conpty::tests::reports_when_passthrough_is_disabled_by_environment",
            ])
            .env("PSMUX_NO_PASSTHROUGH", "1")
            .env(CHILD_ENV, "1")
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "child test failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
