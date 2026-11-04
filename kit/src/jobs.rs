//! the kit's scripts run from the window: a build, a rebuild of every game,
//! an update, their lines read as they come for the progress they tell.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Build,
    Rebuild,
    Update,
}

/// where a game is in its build.
#[derive(Clone, PartialEq)]
pub enum Stage {
    Waiting,
    Unpacking,
    Tools,
    Recompiling(u32, u32),
    Packing,
    Done,
}

pub struct GameProgress {
    pub id: String,
    pub name: String,
    pub stage: Stage,
}

pub struct Job {
    pub kind: Kind,
    child: Child,
    lines: Receiver<String>,
    pub log: Vec<String>,
    pub games: Vec<GameProgress>,
    /// how many games the job was given, when it knows.
    pub expected: usize,
    /// the exit, Some(true) when it went through.
    pub finished: Option<bool>,
}

/// the kit's folder: the one the program sits in, or above it, with the
/// scripts in it.
pub fn kit_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("POCKET_NATIVE_KIT") {
        return dir.into();
    }
    let exe = std::env::current_exe().ok();
    let found = exe.as_deref().and_then(Path::parent).and_then(|dir| dir.ancestors().find(|dir| dir.join("scripts").join("build.sh").is_file()).map(Path::to_path_buf));
    found.unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

/// the script for a job on this system, and its arguments.
fn command(kit: &Path, kind: Kind, out: &Path, keys: Option<&Path>, roms: &[PathBuf]) -> Command {
    let script = match kind {
        Kind::Build => "build",
        Kind::Rebuild => "batch-rebuild",
        Kind::Update => "update",
    };
    let mut command;
    if cfg!(windows) {
        command = Command::new("powershell");
        command.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]).arg(kit.join("scripts").join(format!("{script}.ps1"))).arg("-Out").arg(out);
        if let Some(keys) = keys.filter(|_| kind == Kind::Build) {
            command.arg("-Keys").arg(keys);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // no console window over the kit's
            command.creation_flags(0x0800_0000);
        }
    } else {
        command = Command::new("bash");
        command.arg(kit.join("scripts").join(format!("{script}.sh"))).arg("--out").arg(out);
        if let Some(keys) = keys.filter(|_| kind == Kind::Build) {
            command.arg("--keys").arg(keys);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // its own group, which a cancel stops whole, the container's
            // client with it
            command.process_group(0);
        }
    }
    if kind == Kind::Build {
        command.args(roms);
    }
    command.current_dir(kit).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    command
}

impl Job {
    pub fn start(kit: &Path, kind: Kind, out: &Path, keys: Option<&Path>, roms: &[PathBuf]) -> std::io::Result<Job> {
        let mut child = command(kit, kind, out, keys, roms).spawn()?;
        let (send, lines) = channel();
        for stream in [child.stdout.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>), child.stderr.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>)].into_iter().flatten() {
            let send = send.clone();
            std::thread::spawn(move || {
                // progress lines end in a carriage return to be redrawn in
                // place, each one is a line here
                let mut reader = BufReader::new(stream);
                let mut buffer = Vec::new();
                while reader.read_until(b'\n', &mut buffer).is_ok_and(|n| n > 0) {
                    for part in String::from_utf8_lossy(&buffer).split('\r') {
                        let line = part.trim_end().to_owned();
                        if !line.is_empty() && send.send(line).is_err() {
                            return;
                        }
                    }
                    buffer.clear();
                }
            });
        }
        Ok(Job { kind, child, lines, log: Vec::new(), games: Vec::new(), expected: if kind == Kind::Build { roms.len() } else { 0 }, finished: None })
    }

    /// takes in what the script said since, true when something changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(line) = self.lines.try_recv() {
            self.read(&line);
            self.log.push(line);
            changed = true;
        }
        if self.log.len() > 4000 {
            self.log.drain(..self.log.len() - 4000);
        }
        if self.finished.is_none() {
            if let Ok(Some(status)) = self.child.try_wait() {
                self.finished = Some(status.success());
                if status.success() {
                    for game in &mut self.games {
                        game.stage = Stage::Done;
                    }
                }
                changed = true;
            }
        }
        changed
    }

    fn read(&mut self, line: &str) {
        if let Some(rest) = line.strip_prefix("== ") {
            let (first, name) = rest.split_once(' ').unwrap_or((rest, ""));
            if first.len() == 16 && first.chars().all(|c| c.is_ascii_hexdigit()) {
                if let Some(last) = self.games.last_mut() {
                    last.stage = Stage::Done;
                }
                self.games.push(GameProgress { id: first.to_owned(), name: name.to_owned(), stage: Stage::Waiting });
                return;
            }
            let stage = if rest.starts_with("decompressing") || rest.starts_with("decrypting") {
                Some(Stage::Unpacking)
            } else if rest.starts_with("building the recompiler") {
                Some(Stage::Tools)
            } else if rest.starts_with("recompiling") {
                Some(Stage::Recompiling(0, 0))
            } else if rest.starts_with("putting Pocket Native together") || rest.starts_with("finding the game's shaders") {
                Some(Stage::Packing)
            } else if rest.starts_with("done") {
                Some(Stage::Done)
            } else {
                None
            };
            if let (Some(stage), Some(game)) = (stage, self.games.last_mut()) {
                game.stage = stage;
            }
        } else if let Some(rest) = line.strip_prefix("compiled ") {
            let numbers: Vec<u32> = rest.split(" of ").filter_map(|n| n.trim().parse().ok()).collect();
            if let ([done, of], Some(game)) = (numbers.as_slice(), self.games.last_mut()) {
                game.stage = Stage::Recompiling(*done, *of);
            }
        }
    }

    /// how far the whole job is, from 0 to 1.
    pub fn fraction(&self) -> f32 {
        if self.finished == Some(true) {
            return 1.0;
        }
        let total = self.expected.max(self.games.len()).max(1) as f32;
        let done: f32 = self.games.iter().map(|game| stage_fraction(&game.stage)).sum();
        (done / total).min(0.99)
    }

    pub fn cancel(&mut self) {
        #[cfg(unix)]
        {
            let _ = Command::new("kill").args(["-TERM", &format!("-{}", self.child.id())]).status();
        }
        #[cfg(not(unix))]
        {
            let _ = self.child.kill();
        }
    }
}

/// how far a game is through its build: the recompiling most of it.
pub fn stage_fraction(stage: &Stage) -> f32 {
    match stage {
        Stage::Waiting => 0.0,
        Stage::Unpacking => 0.03,
        Stage::Tools => 0.05,
        Stage::Recompiling(done, of) if *of > 0 => 0.1 + 0.8 * *done as f32 / *of as f32,
        Stage::Recompiling(..) => 0.1,
        Stage::Packing => 0.92,
        Stage::Done => 1.0,
    }
}

pub fn stage_text(stage: &Stage) -> String {
    match stage {
        Stage::Waiting => "starting".into(),
        Stage::Unpacking => "unpacking the game".into(),
        Stage::Tools => "building the recompiler".into(),
        Stage::Recompiling(done, of) if *of > 0 => format!("recompiling, {done} of {of} files"),
        Stage::Recompiling(..) => "recompiling".into(),
        Stage::Packing => "putting it together".into(),
        Stage::Done => "done".into(),
    }
}

/// a command run quietly, with no console window of its own on Windows.
fn quiet(program: &str) -> Command {
    let mut command = Command::new(program);
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    command
}

/// the container engine the build uses, and whether it is running.
#[derive(Clone, Copy, PartialEq)]
pub enum Engine {
    Missing,
    Stopped(&'static str),
    Running(&'static str),
}

pub fn engine() -> Engine {
    let order = if cfg!(windows) { ["docker", "podman"] } else { ["podman", "docker"] };
    let Some(name) = order.into_iter().find(|name| quiet(name).arg("--version").status().is_ok_and(|s| s.success())) else { return Engine::Missing };
    if quiet(name).arg("info").status().is_ok_and(|s| s.success()) {
        Engine::Running(name)
    } else {
        Engine::Stopped(name)
    }
}

/// Docker Desktop installed with winget, in a window of its own to follow.
pub fn install_docker() -> std::io::Result<()> {
    Command::new("cmd").args(["/c", "start", "powershell", "-NoExit", "-Command", "winget install -e --id Docker.DockerDesktop; Write-Host 'Start Docker Desktop, then press Check again in Pocket Native.'"]).spawn().map(drop)
}

/// what update's check says: the newer version out, None when this one is
/// the latest, or an error when it could not tell.
pub fn newer_version(kit: &Path) -> Result<Option<String>, ()> {
    let mut command = if cfg!(windows) {
        let mut command = quiet("powershell");
        command.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]).arg(kit.join("scripts").join("update.ps1")).arg("-Check");
        command
    } else {
        let mut command = quiet("bash");
        command.arg(kit.join("scripts").join("update.sh")).arg("--check");
        command
    };
    let output = command.stdout(Stdio::piped()).current_dir(kit).output().map_err(drop)?;
    if !output.status.success() {
        return Err(());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text.lines().find(|line| line.contains(" is out")).and_then(|line| line.split_whitespace().skip_while(|word| *word != "Native").nth(1).map(str::to_owned)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job_reading(lines: &[&str]) -> Job {
        let child = Command::new("true").spawn().expect("true runs");
        let (_send, receiver) = channel();
        let mut job = Job { kind: Kind::Build, child, lines: receiver, log: Vec::new(), games: Vec::new(), expected: 2, finished: None };
        for line in lines {
            job.read(line);
        }
        job
    }

    #[test]
    fn the_build_lines_tell_each_games_progress() {
        let job = job_reading(&[
            "== 0004000000030800 Mario Kart 7",
            "== building the recompiler and Pocket Native",
            "== recompiling the game for the handheld",
            "compiled 36 of 72",
        ]);
        assert_eq!(job.games.len(), 1);
        assert_eq!(job.games[0].name, "Mario Kart 7");
        assert!(job.games[0].stage == Stage::Recompiling(36, 72));
        assert!((job.fraction() - 0.25).abs() < 0.01, "{}", job.fraction());
        let job = job_reading(&["== 0004000000030800 Mario Kart 7", "compiled 72 of 72", "== 00040000000EC300 The Legend of Zelda: A Link Between Worlds"]);
        assert!(job.games[0].stage == Stage::Done && job.games[1].stage == Stage::Waiting);
    }
}
