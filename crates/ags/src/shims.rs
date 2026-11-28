//! The shims that run plain `claude`, `codex` and `pi` through ags, and how ags finds the real
//! CLIs past them.
use std::{
    ffi::OsStr,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use clap::ValueEnum;

use crate::{Cli, Store, store::write_executable};

const DIR_NAME: &str = "bin";
/// The line after the shebang that marks a file as an ags shim.
const MARKER: &str = "# ags shim: `ags login` writes this file and `ags logout` removes it.";
/// The mark of shims that run `ags run <cli>`.
const RUN_MARKER: &str = "# Managed by AGS";
/// Set to run the real CLI without ags for one run.
pub const BYPASS: &str = "AGS_BYPASS";
/// The CLIs ags started, each inside the one before it, with the ags process of each.
pub const LAUNCHED: &str = "AGS_LAUNCHED";
/// The most CLIs ags runs one inside another; it refuses a deeper one, which ends any loop.
pub const MAX_CHAIN: usize = 3;
/// How many processes above ags it looks through for its shell, past wrappers such as `env`.
const ANCESTORS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

impl Shell {
    /// The shell ags runs in: the nearest one above it, else the one `SHELL` names.
    pub fn of_user() -> Option<Self> {
        Self::detect(
            std::process::id(),
            process,
            std::env::var_os("SHELL").as_deref(),
        )
    }

    /// The nearest shell above `pid`, given each process's parent and name, else `login`'s.
    fn detect(
        pid: u32,
        process: impl Fn(u32) -> Option<(u32, String)>,
        login: Option<&OsStr>,
    ) -> Option<Self> {
        Self::ancestor(pid, process).or_else(|| Self::named(login?.to_str()?))
    }

    fn ancestor(pid: u32, process: impl Fn(u32) -> Option<(u32, String)>) -> Option<Self> {
        let mut pid = process(pid)?.0;
        for _ in 0..ANCESTORS {
            if pid == 0 {
                return None;
            }
            let (parent, name) = process(pid)?;
            if let Some(shell) = Self::named(&name) {
                return Some(shell);
            }
            pid = parent;
        }
        None
    }

    /// The shell a program is, by its file name; a login shell's name starts with `-`.
    fn named(program: &str) -> Option<Self> {
        let name = program.rsplit('/').next()?.trim_start_matches('-');
        Self::from_str(name, false).ok()
    }

    /// The line that runs `ags init` for this shell.
    pub fn init_line(self) -> &'static str {
        match self {
            Self::Bash => "eval \"$(ags init bash)\"",
            Self::Zsh => "eval \"$(ags init zsh)\"",
            Self::Fish => "ags init fish | source",
        }
    }

    pub fn startup_file(self) -> &'static str {
        match self {
            Self::Bash => "~/.bashrc",
            Self::Zsh => "~/.zshrc",
            Self::Fish => "~/.config/fish/config.fish",
        }
    }
}

/// The dir of ags's shims.
pub fn dir(store: &Store) -> PathBuf {
    store.dir().join(DIR_NAME)
}

/// Shell code that puts `dir` first on `PATH`, once however often it runs.
pub fn init(shell: Shell, dir: &Path) -> Result<String> {
    let Some(dir) = dir.to_str() else {
        bail!("{} is not a path a shell can be given", dir.display());
    };
    Ok(match shell {
        Shell::Bash => {
            let dir = sh_quote(dir);
            format!(
                "_ags_path=\":$PATH:\"\n\
                 while [[ $_ags_path == *:{dir}:* ]]; do _ags_path=${{_ags_path/:{dir}:/:}}; done\n\
                 _ags_path=${{_ags_path#:}}; _ags_path=${{_ags_path%:}}\n\
                 export PATH={dir}${{_ags_path:+:$_ags_path}}\n\
                 unset _ags_path\n"
            )
        }
        Shell::Zsh => {
            let dir = sh_quote(dir);
            format!(
                "() {{\n\
                 \x20 local dir=$1 entry\n\
                 \x20 local -a kept\n\
                 \x20 [[ -n $PATH ]] || path=()\n\
                 \x20 for entry in \"${{path[@]}}\"; do [[ $entry == \"$dir\" ]] || kept+=(\"$entry\"); done\n\
                 \x20 path=(\"$dir\" \"${{kept[@]}}\")\n\
                 }} {dir}\n"
            )
        }
        Shell::Fish => {
            let dir = fish_quote(dir);
            format!(
                "while set -l i (contains -i -- {dir} $PATH); set -e PATH[$i]; end\n\
                 set -gx PATH {dir} $PATH\n"
            )
        }
    })
}

fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn fish_quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// The shim of `cli`, in `dir`: `ags` runs the CLI, else the `ags` on `PATH`.
fn shim(cli: Cli, ags: &Path, dir: &Path) -> Result<String> {
    let (Some(ags), Some(dir)) = (ags.to_str(), dir.to_str()) else {
        bail!("{} is not a path a shim can run", ags.display());
    };
    let missing = sh_quote(&format!(
        "{cli}: ags is not installed; install it, or delete {dir} to run {cli} without it"
    ));
    Ok(format!(
        "#!/bin/sh\n\
         {MARKER}\n\
         ags={}\n\
         [ -f \"$ags\" ] && [ -x \"$ags\" ] || ags=$(command -v ags) || {{ printf '%s\\n' {missing} >&2; exit 127; }}\n\
         exec \"$ags\" --shim {cli} \"$@\"\n",
        sh_quote(ags)
    ))
}

/// One CLI that ags started, and the ags process that started it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entered {
    pub cli: Cli,
    pub pid: u32,
}

/// The CLIs ags started that this process runs inside, outermost first; none once the last ended.
pub fn chain() -> Vec<Entered> {
    let chain = parse_chain(&std::env::var(LAUNCHED).unwrap_or_default());
    match chain.last() {
        Some(last) if running(last.pid) => chain,
        _ => vec![],
    }
}

fn parse_chain(value: &str) -> Vec<Entered> {
    value
        .split(',')
        .filter_map(|entry| {
            let (cli, pid) = entry.split_once(':')?;
            Some(Entered {
                cli: Cli::from_str(cli, false).ok()?,
                pid: pid.parse().ok()?,
            })
        })
        .collect()
}

/// `LAUNCHED` for `cli`, which this process starts inside `chain`.
pub fn entered(chain: &[Entered], cli: Cli) -> String {
    chain
        .iter()
        .copied()
        .chain([Entered {
            cli,
            pid: std::process::id(),
        }])
        .map(|entered| format!("{}:{}", entered.cli, entered.pid))
        .collect::<Vec<_>>()
        .join(",")
}

/// Starting `cli` inside `chain` would go deeper than ags allows.
#[derive(Debug)]
pub struct TooDeep(Cli);

impl std::fmt::Display for TooDeep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} would be {} CLIs deep through ags, which looks like a loop; set AGS_BYPASS=1 to run it anyway",
            self.0,
            MAX_CHAIN + 1
        )
    }
}

impl std::error::Error for TooDeep {}

/// Refuses `cli` inside a chain that is already as deep as ags allows.
pub fn check_depth(chain: &[Entered], cli: Cli) -> Result<()> {
    if chain.len() >= MAX_CHAIN {
        return Err(TooDeep(cli).into());
    }
    Ok(())
}

/// Whether process `pid` is still running.
pub(crate) fn running(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // SAFETY: signal 0 only checks that the process exists.
        let found = unsafe { libc::kill(pid, 0) } == 0;
        found || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// The parent and program name of process `pid`.
#[cfg(target_os = "linux")]
fn process(pid: u32) -> Option<(u32, String)> {
    let parent = stat_parent(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)?;
    let name = fs::read(format!("/proc/{pid}/comm")).ok()?;
    Some((
        parent,
        String::from_utf8_lossy(&name)
            .trim_end_matches('\n')
            .to_owned(),
    ))
}

/// The parent in a `/proc/<pid>/stat`, whose fields follow the name's last `)`.
#[cfg(target_os = "linux")]
fn stat_parent(stat: &str) -> Option<u32> {
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// The parent and program name of process `pid`.
#[cfg(target_os = "macos")]
fn process(pid: u32) -> Option<(u32, String)> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-o", "ppid=,comm=", "-p", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let (parent, name) = text.trim().split_once(char::is_whitespace)?;
    Some((parent.parse().ok()?, name.trim_start().to_owned()))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process(_pid: u32) -> Option<(u32, String)> {
    None
}

/// Whether every shim is in place and runs `ags`.
pub fn current(store: &Store, ags: &Path) -> bool {
    let dir = dir(store);
    Cli::value_variants().iter().all(|&cli| {
        shim(cli, ags, &dir)
            .is_ok_and(|shim| fs::read(dir.join(cli.to_string())).ok() == Some(shim.into_bytes()))
    })
}

/// Writes a shim running `ags` for every CLI, or none when a file in the way is not one.
pub fn install(store: &Store, ags: &Path) -> Result<()> {
    // ags runs no shims on Windows.
    if !cfg!(unix) {
        return Ok(());
    }
    let dir = dir(store);
    let mut shims = vec![];
    for &cli in Cli::value_variants() {
        let path = dir.join(cli.to_string());
        if fs::symlink_metadata(&path).is_ok() && !is_shim(&path) {
            bail!(
                "ags wrote no shims: {} is not one of them; move it away, then run `ags init <shell>`",
                path.display()
            );
        }
        shims.push((cli.to_string(), shim(cli, ags, &dir)?));
    }
    for (name, shim) in shims {
        write_executable(&dir, &name, shim.as_bytes())?;
    }
    Ok(())
}

/// Removes the shims, and their dir once it is empty.
pub fn remove(store: &Store) -> Result<()> {
    let dir = dir(store);
    for &cli in Cli::value_variants() {
        let path = dir.join(cli.to_string());
        if !is_shim(&path) {
            continue;
        }
        match fs::remove_file(&path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                return Err(error).with_context(|| format!("removing {}", path.display()));
            }
            _ => {}
        }
    }
    let _ = fs::remove_dir(&dir);
    Ok(())
}

/// Whether `dir` holds a shim for every CLI.
pub fn written(dir: &Path) -> bool {
    Cli::value_variants()
        .iter()
        .all(|cli| is_shim(&dir.join(cli.to_string())))
}

/// Whether `path` is a shim of this or an earlier ags.
pub fn is_shim(path: &Path) -> bool {
    let mut head = [0_u8; 256];
    let Ok(read) = fs::File::open(path).and_then(|mut file| file.read(&mut head)) else {
        return false;
    };
    let mut lines = head[..read].split(|byte| *byte == b'\n');
    lines.next().is_some_and(|line| line.starts_with(b"#!"))
        && lines
            .next()
            .is_some_and(|line| line == MARKER.as_bytes() || line == RUN_MARKER.as_bytes())
}

/// Each executable named `program` in the dirs of `path`, a value of `PATH`, in order.
pub(crate) fn executables(program: &str, path: Option<&OsStr>) -> Vec<PathBuf> {
    let Some(path) = path else {
        return vec![];
    };
    let names: Vec<String> = if cfg!(windows) {
        ["exe", "cmd", "bat"]
            .map(|extension| format!("{program}.{extension}"))
            .to_vec()
    } else {
        vec![program.to_owned()]
    };
    std::env::split_paths(path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .filter(|candidate| executable(candidate))
        .filter_map(|candidate| std::path::absolute(candidate).ok())
        .collect()
}

fn executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    path.is_file()
}

fn same_dir(a: &Path, b: &Path) -> bool {
    a == b || fs::canonicalize(a).is_ok_and(|a| fs::canonicalize(b).is_ok_and(|b| a == b))
}

/// The first `cli` on `path` that is neither in `shims` nor any ags shim.
fn find(cli: Cli, shims: &Path, path: Option<&OsStr>) -> Option<PathBuf> {
    executables(&cli.to_string(), path)
        .into_iter()
        .find(|candidate| {
            !candidate.parent().is_some_and(|dir| same_dir(dir, shims)) && !is_shim(candidate)
        })
}

/// The real `cli` on this process's `PATH`, which is how ags finds every CLI it runs.
pub fn program(store: &Store, cli: Cli) -> Result<PathBuf> {
    program_on(store, cli, std::env::var_os("PATH").as_deref())
}

/// The real `cli` on `path`, a value of `PATH`.
pub(crate) fn program_on(store: &Store, cli: Cli, path: Option<&OsStr>) -> Result<PathBuf> {
    find(cli, &dir(store), path)
        .ok_or_else(|| anyhow!("could not find {cli}; install it or add it to PATH"))
}

/// Whether `path` runs each CLI's shim in `shims` before any other of its name.
pub fn active(shims: &Path, path: Option<&OsStr>) -> bool {
    Cli::value_variants().iter().all(|cli| {
        executables(&cli.to_string(), path)
            .first()
            .and_then(|first| first.parent())
            .is_some_and(|dir| same_dir(dir, shims))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::scratch;

    #[cfg(unix)]
    fn script(path: &Path, text: &str) {
        use std::os::unix::fs::PermissionsExt;
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn joined(dirs: &[&Path]) -> std::ffi::OsString {
        std::env::join_paths(dirs).unwrap()
    }

    /// `shell` from this process's `PATH`, if it is installed.
    fn shell(name: &str) -> Option<PathBuf> {
        executables(name, std::env::var_os("PATH").as_deref())
            .into_iter()
            .next()
    }

    #[cfg(unix)]
    #[test]
    fn login_writes_a_shim_for_each_cli_and_logout_removes_only_those() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch();
        let store = Store::at(root.join("ags"));
        let ags = Path::new("/opt/it's/ags");
        install(&store, ags).unwrap();
        install(&store, ags).unwrap();
        let shims = dir(&store);
        let quoted = |dir: &Path| format!("{}", dir.display()).replace('\'', "'\\''");
        assert_eq!(
            fs::read_to_string(shims.join("codex")).unwrap(),
            format!(
                "#!/bin/sh\n{MARKER}\nags='/opt/it'\\''s/ags'\n\
                 [ -f \"$ags\" ] && [ -x \"$ags\" ] || ags=$(command -v ags) || {{ printf '%s\\n' 'codex: ags is not installed; install it, or delete {} to run codex without it' >&2; exit 127; }}\n\
                 exec \"$ags\" --shim codex \"$@\"\n",
                quoted(&shims)
            )
        );
        for cli in Cli::value_variants() {
            let path = shims.join(cli.to_string());
            assert!(is_shim(&path), "{cli}");
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        assert!(written(&shims));
        assert_eq!(
            fs::read_dir(&shims).unwrap().count(),
            3,
            "no staging file is left"
        );

        fs::write(shims.join("notes"), "mine").unwrap();
        remove(&store).unwrap();
        assert_eq!(
            fs::read_dir(&shims)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>(),
            ["notes"]
        );
        fs::remove_file(shims.join("notes")).unwrap();
        remove(&store).unwrap();
        assert!(!shims.exists(), "an empty shim dir goes too");

        script(&shims.join("pi"), "#!/bin/sh\necho mine\n");
        let refused = install(&store, ags).unwrap_err().to_string();
        assert_eq!(
            refused,
            format!(
                "ags wrote no shims: {} is not one of them; move it away, then run `ags init <shell>`",
                shims.join("pi").display()
            )
        );
        assert!(!shims.join("claude").exists(), "none is written");
        assert!(!written(&shims));
        remove(&store).unwrap();
        assert!(
            shims.join("pi").exists(),
            "logout keeps a file it did not write"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_shim_runs_its_own_ags_else_the_one_on_path_else_says_what_to_do() {
        let root = scratch();
        let store = Store::at(root.join("ags"));
        let own = root.join("own/ags");
        script(&own, "#!/bin/sh\necho \"own ags $*\"\n");
        let other = root.join("other");
        script(&other.join("ags"), "#!/bin/sh\necho \"other ags $*\"\n");
        install(&store, &own).unwrap();
        let shims = dir(&store);
        let sh = shell("sh").unwrap();
        let run = |dirs: &[&Path]| {
            let output = std::process::Command::new(&sh)
                .arg(shims.join("pi"))
                .args(["a b", "c"])
                .env("PATH", joined(dirs))
                .output()
                .unwrap();
            (
                output.status.code(),
                String::from_utf8(output.stdout).unwrap(),
                String::from_utf8(output.stderr).unwrap(),
            )
        };
        let tools = Path::new("/usr/bin");
        assert_eq!(
            run(&[&other, tools]),
            (Some(0), "own ags --shim pi a b c\n".into(), String::new())
        );
        fs::remove_file(&own).unwrap();
        assert_eq!(
            run(&[&other, tools]),
            (Some(0), "other ags --shim pi a b c\n".into(), String::new())
        );
        assert_eq!(
            run(&[tools]),
            (
                Some(127),
                String::new(),
                format!(
                    "pi: ags is not installed; install it, or delete {} to run pi without it\n",
                    shims.display()
                )
            )
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_real_cli_is_found_past_every_shim() {
        let root = scratch();
        let store = Store::at(root.join("ags"));
        install(&store, Path::new("/usr/bin/ags")).unwrap();
        let shims = dir(&store);
        let run = root.join("run");
        script(
            &run.join("claude"),
            &format!("#!/bin/sh\n{RUN_MARKER}\nexec ags run claude -- \"$@\"\n"),
        );
        let linked = root.join("linked");
        fs::create_dir_all(&linked).unwrap();
        std::os::unix::fs::symlink(shims.join("claude"), linked.join("claude")).unwrap();
        let other = root.join("other");
        script(
            &other.join("claude"),
            "#!/bin/sh\nexec node claude.js \"$@\"\n",
        );
        let real = root.join("real");
        script(&real.join("claude"), "#!/bin/sh\necho real\n");

        let path = joined(&[&shims, &run, &linked, &real, &other]);
        assert_eq!(
            find(Cli::Claude, &shims, Some(&path)),
            Some(real.join("claude"))
        );
        let path = joined(&[&shims, &other, &real]);
        assert_eq!(
            find(Cli::Claude, &shims, Some(&path)),
            Some(other.join("claude"))
        );
        let path = joined(&[&shims, &run]);
        assert_eq!(find(Cli::Claude, &shims, Some(&path)), None);
        assert_eq!(find(Cli::Claude, &shims, None), None);

        // The shim dir is skipped even before its shims are written.
        remove(&store).unwrap();
        script(&shims.join("claude"), "#!/bin/sh\nexec ags claude \"$@\"\n");
        let path = joined(&[&shims, &real]);
        assert_eq!(
            find(Cli::Claude, &shims, Some(&path)),
            Some(real.join("claude"))
        );

        let cwd = std::env::current_dir().unwrap();
        let mut relative = PathBuf::new();
        for _ in cwd.components().skip(1) {
            relative.push("..");
        }
        let relative = relative.join(real.strip_prefix("/").unwrap());
        let found = find(Cli::Claude, &shims, Some(&joined(&[&relative]))).unwrap();
        assert!(found.is_absolute(), "{}", found.display());
        assert_eq!(
            fs::canonicalize(found).unwrap(),
            fs::canonicalize(real.join("claude")).unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_shims_are_active_once_they_come_before_every_real_cli() {
        let root = scratch();
        let store = Store::at(root.join("ags"));
        let shims = dir(&store);
        let real = root.join("real");
        script(&real.join("codex"), "#!/bin/sh\n");
        assert!(
            !active(&shims, Some(&joined(&[&shims, &real]))),
            "no shims yet"
        );
        install(&store, Path::new("/usr/bin/ags")).unwrap();
        assert!(active(&shims, Some(&joined(&[&shims, &real]))));
        assert!(!active(&shims, Some(&joined(&[&real, &shims]))));
        assert!(!active(&shims, Some(&joined(&[&real]))));
        assert!(!active(&shims, None));
        fs::remove_dir_all(root).unwrap();
    }

    /// Runs each shell's init code twice in that shell, when it is installed, from several `PATH`s.
    #[cfg(unix)]
    #[test]
    fn the_init_code_puts_the_shims_first_on_path_once() {
        let root = scratch();
        let shims = root.join("it's a $bin [*]");
        let dir = shims.display().to_string();
        let cases = [
            ("/a:/b".to_owned(), format!("{dir}:/a:/b")),
            (format!("/a:{dir}:{dir}:/b"), format!("{dir}:/a:/b")),
            (format!("{dir}:/a"), format!("{dir}:/a")),
            (format!("/a:{dir}x:/b"), format!("{dir}:/a:{dir}x:/b")),
        ];
        let mut ran = vec![];
        for (shell, name, args, print, empty_entries) in [
            (
                Shell::Bash,
                "bash",
                &["--norc", "--noprofile", "-c"][..],
                "printf %s \"$PATH\"",
                true,
            ),
            (
                Shell::Zsh,
                "zsh",
                &["-f", "-c"][..],
                "print -rn -- \"$PATH\"",
                true,
            ),
            (
                Shell::Fish,
                "fish",
                &["--no-config", "-c"][..],
                "printf %s (string join : $PATH)",
                false,
            ),
        ] {
            let Some(program) = self::shell(name) else {
                assert_ne!(shell, Shell::Bash, "bash runs the test");
                continue;
            };
            ran.push(shell);
            let code = init(shell, &shims).unwrap();
            // fish reads an empty entry of PATH as the current dir.
            let empty = [
                ("/a::/b".to_owned(), format!("{dir}:/a::/b")),
                (String::new(), dir.clone()),
            ];
            let empty = if empty_entries { &empty[..] } else { &[] };
            for (before, after) in cases.iter().chain(empty) {
                let output = std::process::Command::new(&program)
                    .args(args)
                    .arg(format!("{code}{code}{print}"))
                    .env("PATH", before)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{name}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(
                    String::from_utf8(output.stdout).unwrap(),
                    *after,
                    "{name} from {before:?}"
                );
            }
        }
        assert!(ran.contains(&Shell::Bash));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_chain_names_each_cli_and_its_ags_and_ends_with_the_last_one() {
        let me = std::process::id();
        assert_eq!(
            parse_chain(&format!("claude:{me},codex:1,bogus:2,pi,1")),
            [
                Entered {
                    cli: Cli::Claude,
                    pid: me
                },
                Entered {
                    cli: Cli::Codex,
                    pid: 1
                },
            ]
        );
        assert!(running(me));
        let mut ended = std::process::Command::new("true").spawn().unwrap();
        let gone = ended.id();
        ended.wait().unwrap();
        assert!(!running(gone));
        let chain = parse_chain(&format!("claude:{me}"));
        assert_eq!(entered(&chain, Cli::Pi), format!("claude:{me},pi:{me}"));
        assert!(check_depth(&chain, Cli::Pi).is_ok());
        let deep = parse_chain(&format!("claude:{me},codex:{me},pi:{me}"));
        let error = check_depth(&deep, Cli::Claude).unwrap_err();
        assert!(error.is::<TooDeep>());
        assert_eq!(
            error.to_string(),
            "claude would be 4 CLIs deep through ags, which looks like a loop; set AGS_BYPASS=1 to run it anyway"
        );
    }

    /// Processes as `(pid, parent, name)`, read the way `process` reads one.
    fn tree(processes: &[(u32, u32, &str)]) -> impl Fn(u32) -> Option<(u32, String)> {
        let processes: Vec<_> = processes
            .iter()
            .map(|&(pid, parent, name)| (pid, parent, name.to_owned()))
            .collect();
        move |pid| {
            processes
                .iter()
                .find(|process| process.0 == pid)
                .map(|(_, parent, name)| (*parent, name.clone()))
        }
    }

    #[test]
    fn the_shell_is_the_nearest_one_above_ags_past_any_wrapper() {
        let bash = Some(OsStr::new("/bin/bash"));
        let fish = [
            (1, 0, "systemd"),
            (10, 1, "bash"),
            (20, 10, "fish"),
            (30, 20, "ags"),
        ];
        assert_eq!(Shell::detect(30, tree(&fish), bash), Some(Shell::Fish));
        let wrapped = [
            (1, 0, "systemd"),
            (20, 1, "-zsh"),
            (21, 20, "sudo"),
            (22, 21, "env"),
            (23, 22, "mise"),
            (30, 23, "ags"),
        ];
        assert_eq!(Shell::detect(30, tree(&wrapped), bash), Some(Shell::Zsh));
        let named = [(20, 1, "/usr/local/bin/fish"), (30, 20, "ags")];
        assert_eq!(Shell::detect(30, tree(&named), None), Some(Shell::Fish));
        let ags_is_no_shell = [(20, 1, "bash"), (30, 20, "fish")];
        assert_eq!(
            Shell::detect(30, tree(&ags_is_no_shell), None),
            Some(Shell::Bash)
        );
    }

    #[test]
    fn without_a_shell_above_ags_shell_names_it_else_none_does() {
        let fish = Some(OsStr::new("/usr/bin/fish"));
        let none = [(1, 0, "init"), (20, 1, "sshd"), (30, 20, "ags")];
        assert_eq!(Shell::detect(30, tree(&none), fish), Some(Shell::Fish));
        assert_eq!(Shell::detect(30, tree(&none), None), None);
        assert_eq!(
            Shell::detect(30, tree(&none), Some(OsStr::new("/bin/tcsh"))),
            None
        );
        let unreadable = [(20, 19, "env"), (30, 20, "ags")];
        assert_eq!(
            Shell::detect(30, tree(&unreadable), fish),
            Some(Shell::Fish)
        );
        assert_eq!(Shell::detect(30, |_| None, fish), Some(Shell::Fish));
    }

    #[test]
    fn the_walk_ends_within_its_bound() {
        let looked = std::cell::Cell::new(0);
        let endless = |pid: u32| {
            looked.set(looked.get() + 1);
            Some((pid + 1, "env".to_owned()))
        };
        assert_eq!(Shell::detect(30, endless, None), None);
        assert_eq!(looked.get(), ANCESTORS + 1);
        // ags is process 100, under wrappers, with bash `depth` processes above it.
        let bash_above = |depth: u32| {
            let processes: Vec<_> = (100 - depth..=100)
                .map(|pid| {
                    (
                        pid,
                        pid - 1,
                        if pid == 100 - depth { "bash" } else { "env" },
                    )
                })
                .collect();
            Shell::detect(100, tree(&processes), None)
        };
        assert_eq!(bash_above(ANCESTORS as u32), Some(Shell::Bash));
        assert_eq!(bash_above(ANCESTORS as u32 + 1), None);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_process_is_read_with_its_parent_and_name() {
        let me = std::process::id();
        assert_eq!(
            process(me).map(|(parent, _)| parent),
            Some(std::os::unix::process::parent_id())
        );
        let mut child = std::process::Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // The kernel can resume the parent before it renames the child; an echo shows `cat` runs.
        let mut stdin = child.stdin.take().unwrap();
        io::Write::write_all(&mut stdin, b"x").unwrap();
        child.stdout.take().unwrap().read_exact(&mut [0]).unwrap();
        let read = process(child.id());
        child.kill().unwrap();
        child.wait().unwrap();
        let (parent, name) = read.unwrap();
        assert_eq!(parent, me);
        assert_eq!(name.rsplit('/').next(), Some("cat"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_stat_gives_the_parent_past_any_name() {
        assert_eq!(stat_parent("42 (bash) S 7 42 42 0"), Some(7));
        assert_eq!(stat_parent("42 (a) S 9 (b)) R 7 42 42 0"), Some(7));
        assert_eq!(stat_parent("42 (bash"), None);
    }

    #[test]
    fn each_shell_has_its_init_line() {
        assert_eq!(Shell::Bash.init_line(), "eval \"$(ags init bash)\"");
        assert_eq!(Shell::Zsh.init_line(), "eval \"$(ags init zsh)\"");
        assert_eq!(Shell::Fish.init_line(), "ags init fish | source");
    }
}
