use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, Command as ProcessCommand, Stdio},
    sync::mpsc,
    thread::{self, JoinHandle},
};

use anyhow::{Context, Result, anyhow};

use crate::util::shell_quote;

pub(crate) struct SshCommandOutput {
    pub(crate) success: bool,
    pub(crate) status: String,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

pub(crate) struct SshStreamOutput {
    pub(crate) success: bool,
    pub(crate) status: String,
}

pub(crate) fn ssh_capture(host: &str, command: &str) -> Result<String> {
    let output = ssh_output(host, command, None)?;
    if output.success {
        Ok(output.stdout)
    } else {
        Err(anyhow!(
            "ssh {host} failed with {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            output.stdout.trim(),
            output.stderr.trim()
        ))
    }
}

pub(crate) fn ssh_output(
    host: &str,
    command: &str,
    stdin: Option<&str>,
) -> Result<SshCommandOutput> {
    let (child, stdin_writer) = spawn_ssh(host, command, stdin)?;
    let output = child
        .wait_with_output()
        .with_context(|| format!("failed to wait for ssh {host}"))?;
    join_stdin_writer(host, stdin_writer)?;
    Ok(SshCommandOutput {
        success: output.status.success(),
        status: output.status.to_string(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

pub(crate) fn ssh_stream_lines(
    host: &str,
    command: &str,
    stdin: Option<&str>,
    mut on_line: impl FnMut(&str),
) -> Result<SshStreamOutput> {
    let (mut child, stdin_writer) = spawn_ssh(host, command, stdin)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("ssh stdout was not captured for {host}"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("ssh stderr was not captured for {host}"))?;
    let (sender, receiver) = mpsc::channel();
    let stdout_reader = spawn_line_reader(BufReader::new(stdout), sender.clone());
    let stderr_reader = spawn_line_reader(BufReader::new(stderr), sender);

    for line in receiver {
        on_line(&line);
    }

    join_line_reader(host, "stdout", stdout_reader)?;
    join_line_reader(host, "stderr", stderr_reader)?;
    let status = child
        .wait()
        .with_context(|| format!("failed to wait for ssh {host}"))?;
    join_stdin_writer(host, stdin_writer)?;
    Ok(SshStreamOutput {
        success: status.success(),
        status: status.to_string(),
    })
}

type StdinWriter = JoinHandle<std::io::Result<()>>;

fn spawn_ssh(
    host: &str,
    command: &str,
    stdin: Option<&str>,
) -> Result<(Child, Option<StdinWriter>)> {
    let remote = format!("sh -c {}", shell_quote(command));
    let mut child = ProcessCommand::new("ssh")
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=8"])
        .arg("--")
        .arg(host)
        .arg(remote)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to start ssh for {host}"))?;

    let stdin_writer = if let Some(stdin) = stdin
        && let Some(mut child_stdin) = child.stdin.take()
    {
        let input = stdin.as_bytes().to_vec();
        Some(thread::spawn(move || child_stdin.write_all(&input)))
    } else {
        None
    };
    Ok((child, stdin_writer))
}

fn join_stdin_writer(host: &str, stdin_writer: Option<StdinWriter>) -> Result<()> {
    if let Some(writer) = stdin_writer {
        match writer.join() {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                return Err(err).with_context(|| format!("failed to write ssh stdin for {host}"));
            }
            Err(_) => return Err(anyhow!("ssh stdin writer panicked for {host}")),
        }
    }
    Ok(())
}

fn spawn_line_reader(
    reader: impl BufRead + Send + 'static,
    sender: mpsc::Sender<String>,
) -> JoinHandle<std::io::Result<()>> {
    thread::spawn(move || {
        for line in reader.lines() {
            if sender.send(line?).is_err() {
                break;
            }
        }
        Ok(())
    })
}

fn join_line_reader(
    host: &str,
    stream: &str,
    reader: JoinHandle<std::io::Result<()>>,
) -> Result<()> {
    match reader.join() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(err).with_context(|| format!("failed to read ssh {stream} for {host}")),
        Err(_) => Err(anyhow!("ssh {stream} reader panicked for {host}")),
    }
}
