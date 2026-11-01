use std::{
    env, fs,
    fs::OpenOptions,
    io,
    os::{
        unix::process::ExitStatusExt,
        unix::{
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            process::CommandExt,
        },
    },
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};

#[derive(Clone)]
struct UserCredentials {
    uid: libc::uid_t,
    gid: libc::gid_t,
    groups: Vec<libc::gid_t>,
}

pub fn install(dir: &Path) -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("installation requires root; run `sudo pareto install`");
    }

    let dir = dir
        .canonicalize()
        .with_context(|| format!("failed to resolve install directory {}", dir.display()))?;
    let metadata = fs::metadata(&dir)
        .with_context(|| format!("failed to inspect install directory {}", dir.display()))?;
    if !metadata.is_dir() || metadata.uid() != 0 || metadata.permissions().mode() & 0o022 != 0 {
        bail!(
            "install directory must be a root-owned directory that is not world-writable: {}",
            dir.display()
        );
    }

    let source = env::current_exe().context("failed to locate the running executable")?;
    let destination = dir.join("pareto");
    if let Ok(metadata) = fs::symlink_metadata(&destination) {
        if !metadata.file_type().is_file() {
            bail!(
                "refusing to replace non-regular install path {}",
                destination.display()
            );
        }
    }

    let temporary = dir.join(format!(".pareto.install.{}", std::process::id()));
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&temporary)
        .with_context(|| format!("failed to create {}", temporary.display()))?;

    let copy_result = (|| -> Result<()> {
        io::copy(
            &mut fs::File::open(&source)
                .with_context(|| format!("failed to open executable {}", source.display()))?,
            &mut output,
        )
        .with_context(|| format!("failed to copy executable to {}", temporary.display()))?;
        output.sync_all()?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o4755)).with_context(|| {
            format!(
                "failed to set launcher permissions on {}",
                temporary.display()
            )
        })?;
        fs::rename(&temporary, &destination).with_context(|| {
            format!(
                "failed to install {} as {}",
                source.display(),
                destination.display()
            )
        })?;
        fs::File::open(&dir)?.sync_all()?;
        Ok(())
    })();

    if copy_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    copy_result?;
    println!("installed setuid launcher at {}", destination.display());
    Ok(())
}

pub fn run(data: &Path) -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!(
            "gesture mode requires the installed setuid launcher; run `sudo pareto install` first"
        );
    }

    let credentials = current_user_credentials()?;
    let executable = env::current_exe().context("failed to locate the running executable")?;

    let mut daemon = Command::new(&executable)
        .arg("__gestured")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to start the gesture daemon")?;

    let mut ui = Command::new(&executable);
    ui.arg("__ui")
        .arg("--dict-data")
        .arg(data)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    unsafe {
        ui.pre_exec({
            let credentials = credentials.clone();
            move || drop_privileges(&credentials)
        });
    }

    let mut ui = match ui.spawn().context("failed to start the user interface") {
        Ok(child) => child,
        Err(error) => {
            stop_child(&mut daemon)?;
            return Err(error);
        }
    };

    let outcome = wait_for_child_exit(&mut daemon, &mut ui)?;
    stop_child(&mut daemon)?;
    stop_child(&mut ui)?;

    match outcome {
        ChildOutcome::Success => Ok(()),
        ChildOutcome::Failure(name, status) => {
            bail!("{name} exited unsuccessfully: {status}")
        }
    }
}

pub fn drop_to_real_user() -> Result<()> {
    let credentials = current_user_credentials()?;
    if unsafe { libc::geteuid() } == 0 && credentials.uid != 0 {
        drop_privileges(&credentials).context("failed to drop launcher privileges")?;
    }
    Ok(())
}

fn current_user_credentials() -> Result<UserCredentials> {
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    if count < 0 {
        return Err(io::Error::last_os_error()).context("failed to query supplementary groups");
    }

    let mut groups = vec![0; count as usize];
    if count > 0 {
        let written = unsafe { libc::getgroups(groups.len() as i32, groups.as_mut_ptr()) };
        if written < 0 {
            return Err(io::Error::last_os_error()).context("failed to read supplementary groups");
        }
        groups.truncate(written as usize);
    }
    if !groups.contains(&gid) {
        groups.push(gid);
    }

    Ok(UserCredentials { uid, gid, groups })
}

fn drop_privileges(credentials: &UserCredentials) -> io::Result<()> {
    if unsafe { libc::setgroups(credentials.groups.len(), credentials.groups.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::setgid(credentials.gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::setuid(credentials.uid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

enum ChildOutcome {
    Success,
    Failure(&'static str, String),
}

fn wait_for_child_exit(daemon: &mut Child, ui: &mut Child) -> Result<ChildOutcome> {
    loop {
        if let Some(status) = daemon
            .try_wait()
            .context("failed to inspect gesture daemon")?
        {
            return Ok(if status.success() {
                ChildOutcome::Success
            } else {
                ChildOutcome::Failure("gesture daemon", describe_status(status))
            });
        }
        if let Some(status) = ui.try_wait().context("failed to inspect user interface")? {
            return Ok(if status.success() {
                ChildOutcome::Success
            } else {
                ChildOutcome::Failure("user interface", describe_status(status))
            });
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn describe_status(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => format!(
            "terminated by signal {}",
            status.signal().unwrap_or_default()
        ),
    }
}

fn stop_child(child: &mut Child) -> Result<()> {
    if child.try_wait()?.is_none() {
        child.kill().context("failed to stop child process")?;
    }
    let _ = child.wait();
    Ok(())
}

pub async fn run_gestured() -> Result<()> {
    crate::gestured::run().await
}
