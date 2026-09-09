//! Lifecycle and bounded diagnostics for background inference subprocesses.

use std::{
    io::Read,
    process::{Child, Command},
};

/// Isolates the worker subprocess so cancellation also reaches wrapper children.
pub(super) fn spawn_process_group(command: &mut Command) -> std::io::Result<Child> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    command.spawn()
}

pub(super) fn terminate_process_tree(child: &mut Child) {
    #[cfg(unix)]
    // SAFETY: the child was placed in a new process group whose ID is its PID.
    // Sending SIGKILL to the negative ID targets only that group.
    unsafe {
        let _ = libc::kill(-child.id().cast_signed(), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = child.kill();
    let _ = child.wait();
}

pub(super) fn read_bounded_output(input: impl Read) -> Vec<u8> {
    read_output_chunks(input, |_| {})
}

/// Retains the final diagnostic bytes while streaming fixed-size chunks to a reader.
pub(super) fn read_output_chunks(mut input: impl Read, mut on_chunk: impl FnMut(&[u8])) -> Vec<u8> {
    const LIMIT: usize = 256 * 1024;
    let mut output = Vec::new();
    let mut chunk = [0_u8; 8 * 1024];
    while let Ok(read) = input.read(&mut chunk) {
        if read == 0 {
            break;
        }
        on_chunk(&chunk[..read]);
        output.extend_from_slice(&chunk[..read]);
        if output.len() > LIMIT {
            output.drain(..output.len() - LIMIT);
        }
    }
    output
}

pub(super) fn last_output_line(output: &[u8]) -> Option<&str> {
    std::str::from_utf8(output)
        .ok()?
        .lines()
        .rev()
        .find_map(|line| (!line.trim().is_empty()).then_some(line.trim()))
}
