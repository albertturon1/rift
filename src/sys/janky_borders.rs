//! A small client for JankyBorders (`borders`), the program that draws the focused window's border.
//!
//! Rift owns the decision (see `layout_engine::border_rule`); this module only delivers it. JankyBorders reads
//! NUL-separated settings from one out-of-line Mach message sent to the bootstrap service `git.felix.borders`, the
//! same message its own command line client sends (JankyBorders 1.9.0, `src/mach.c`). Sending is fire and forget
//! and never blocks the reactor for longer than [`SEND_TIMEOUT_MS`].

use std::ffi::{CString, c_char, c_void};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use tracing::{debug, warn};

use super::mach::{mach_msg, mach_msg_header_t};

const SERVICE_NAME: &str = "git.felix.borders";
const SEND_TIMEOUT_MS: u32 = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    /// No JankyBorders is registered.
    NotRunning,
    /// It is registered but did not take the message in time.
    Failed,
}

/// What the reactor needs from JankyBorders; the tests replace it with a recorder.
pub trait BordersClient: Send {
    fn send(&mut self, args: &[String]) -> Result<(), SendError>;
    /// Start JankyBorders with `initial` as its first settings, then apply `then` as soon as it is registered.
    /// `initial` must carry visible colours: a JankyBorders started with only transparent colours accepts later
    /// messages but never draws a border (seen with 1.9.0), so the hidden state is applied after the start.
    fn start(&mut self, initial: &[String], then: &[String]);
}

/// The wire format: arguments separated by NUL and ended by an empty one.
pub fn payload(args: &[String]) -> Vec<u8> {
    let mut bytes = args.join("\0").into_bytes();
    bytes.extend_from_slice(&[0, 0]);
    bytes
}

#[derive(Default)]
pub struct JankyBorders {
    /// From the config; otherwise the usual Homebrew locations, then `PATH`.
    pub binary: Option<PathBuf>,
}

impl JankyBorders {
    fn binary_path(&self) -> PathBuf {
        if let Some(binary) = &self.binary {
            return binary.clone();
        }
        ["/opt/homebrew/bin/borders", "/usr/local/bin/borders"]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| path.exists())
            .unwrap_or_else(|| PathBuf::from("borders"))
    }
}

impl BordersClient for JankyBorders {
    fn send(&mut self, args: &[String]) -> Result<(), SendError> { send_message(&payload(args)) }

    fn start(&mut self, initial: &[String], then: &[String]) {
        let binary = self.binary_path();
        // Own process group: JankyBorders keeps running if Rift is restarted, and the next Rift finds it registered.
        match Command::new(&binary)
            .args(initial)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
        {
            Ok(child) => {
                debug!(?binary, pid = child.id(), "started JankyBorders");
                drop(child);
            }
            Err(error) => {
                warn!(?binary, %error, "could not start JankyBorders");
                return;
            }
        }
        if then != initial {
            let payload = payload(then);
            // It registers within a fraction of a second; never block the reactor on it.
            std::thread::spawn(move || {
                for _ in 0..50 {
                    std::thread::sleep(Duration::from_millis(20));
                    if send_message(&payload).is_ok() {
                        return;
                    }
                }
            });
        }
    }
}

// Mach message layout, packed to 4 bytes like <mach/message.h>.
#[repr(C, packed(4))]
struct OolDescriptor {
    address: *const c_void,
    // deallocate:8 | copy:8 | pad:8 | type:8
    flags: u32,
    size: u32,
}

#[repr(C, packed(4))]
struct Message {
    header: mach_msg_header_t,
    descriptor_count: u32,
    descriptor: OolDescriptor,
}

const TASK_BOOTSTRAP_PORT: i32 = 4;
const MACH_MSG_TYPE_COPY_SEND: u32 = 19;
const MACH_MSGH_BITS_COMPLEX: u32 = 0x8000_0000;
const MACH_SEND_MSG: i32 = 0x1;
const MACH_SEND_TIMEOUT: i32 = 0x10;
const OOL_VIRTUAL_COPY: u32 = (1 << 24) | (1 << 8); // type OOL, copy VIRTUAL, no deallocate

unsafe extern "C" {
    static mach_task_self_: u32;
    fn task_get_special_port(task: u32, which: i32, port: *mut u32) -> i32;
    fn bootstrap_look_up(bootstrap: u32, name: *const c_char, port: *mut u32) -> i32;
    fn mach_port_deallocate(task: u32, name: u32) -> i32;
}

fn send_message(payload: &[u8]) -> Result<(), SendError> {
    let name = CString::new(SERVICE_NAME).expect("service name has no NUL");
    // SAFETY: plain Mach calls on ports we look up and release here; the message and payload outlive `mach_msg`,
    // which copies the out-of-line buffer (virtual copy) before it returns.
    unsafe {
        let task = mach_task_self_;
        let mut bootstrap = 0u32;
        if task_get_special_port(task, TASK_BOOTSTRAP_PORT, &mut bootstrap) != 0 {
            return Err(SendError::Failed);
        }
        let mut port = 0u32;
        if bootstrap_look_up(bootstrap, name.as_ptr(), &mut port) != 0 || port == 0 {
            return Err(SendError::NotRunning);
        }
        let mut message = Message {
            header: mach_msg_header_t {
                msgh_bits: MACH_MSG_TYPE_COPY_SEND | MACH_MSGH_BITS_COMPLEX,
                msgh_size: size_of::<Message>() as u32,
                msgh_remote_port: port,
                msgh_local_port: 0,
                msgh_voucher_port: 0,
                msgh_id: 0,
            },
            descriptor_count: 1,
            descriptor: OolDescriptor {
                address: payload.as_ptr().cast(),
                flags: OOL_VIRTUAL_COPY,
                size: payload.len() as u32,
            },
        };
        let result = mach_msg(
            &raw mut message.header,
            (MACH_SEND_MSG | MACH_SEND_TIMEOUT) as u32,
            size_of::<Message>() as u32,
            0,
            0,
            SEND_TIMEOUT_MS,
            0,
        );
        mach_port_deallocate(task, port);
        if result == 0 { Ok(()) } else { Err(SendError::Failed) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_payload_is_nul_separated_and_ends_with_an_empty_argument() {
        let args = ["active_color=0x00000000".to_string(), "width=8.0".to_string()];
        assert_eq!(payload(&args), b"active_color=0x00000000\0width=8.0\0\0".to_vec());
        assert_eq!(payload(&[]), vec![0, 0]);
    }

    #[test]
    fn the_message_has_the_size_of_the_c_struct_jankyborders_expects() {
        // 24-byte header + 4-byte descriptor count + 16-byte out-of-line descriptor.
        assert_eq!(size_of::<Message>(), 44);
    }

    /// Sends the arguments in `RIFT_LIVE_ARGS` (space separated) to the JankyBorders that is running. Run by hand:
    /// `RIFT_LIVE_ARGS="active_color=0xffff0000" cargo test --lib live_send -- --ignored --nocapture`.
    #[test]
    #[ignore = "talks to the real JankyBorders and changes the border on screen"]
    fn live_send() {
        let args: Vec<String> = std::env::var("RIFT_LIVE_ARGS")
            .expect("set RIFT_LIVE_ARGS")
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let result = JankyBorders::default().send(&args);
        println!("send {args:?} -> {result:?}");
        assert_eq!(result, Ok(()));
    }
}
