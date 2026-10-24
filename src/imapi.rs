//! Windows での作り方: 実行ファイルと同じフォルダの `folder2DVD_ISO.ps1` を PowerShell で呼び、
//! Windows 標準の IMAPI2 に ISO を作らせる。
//!
//! IMAPI2 を Rust から COM で直接呼ばないのは、同じ処理を PowerShell 版として単独でも使えるようにし、
//! 作り方を 1 か所（ps1）にまとめるため。ps1 を実行ファイルに埋め込んで一時フォルダに書き出す形にしないのは、
//! 一時フォルダからスクリプトを書き出して実行する動きがウイルス対策ソフトに怪しまれやすいため（ken 10/24）。
//! 自前の書き出し（`iso.rs`）を使わないのは、IMAPI2 なら UDF で 4GiB 以上のファイルも入るため。

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use crate::iso::{self, Options, Progress, Report};

pub const SCRIPT_NAME: &str = "folder2DVD_ISO.ps1";
/// 呼ぶ PowerShell を変える(試すとき用。既定は Windows に必ずある Windows PowerShell 5.1)
const INTERPRETER_ENV: &str = "FOLDER2ISO_POWERSHELL";
/// ps1 がこの文を出したら、出力ファイルに書き始めている(中断したら消してよい)
const WRITING: &str = "書き出しています";

/// ps1 が `-Machine` のときに出す 1 行。
#[derive(Debug, PartialEq, Eq)]
enum Line {
    Info(String),
    Warn(String),
    Progress(u64, u64),
    Done(u64),
    Error(String),
    Other(String),
}

fn parse(line: &str) -> Line {
    let line = line.trim_end_matches(['\r', '\n']);
    let Some(rest) = line.strip_prefix("@@") else {
        return Line::Other(line.to_string());
    };
    let (kind, body) = rest.split_once(' ').unwrap_or((rest, ""));
    let numbers: Vec<u64> = body
        .split_whitespace()
        .filter_map(|n| n.parse().ok())
        .collect();
    match (kind, numbers.as_slice()) {
        ("INFO", _) => Line::Info(body.to_string()),
        ("WARN", _) => Line::Warn(body.to_string()),
        ("ERROR", _) => Line::Error(body.to_string()),
        ("PROGRESS", [done, total]) => Line::Progress(*done, *total),
        ("DONE", [bytes]) => Line::Done(*bytes),
        _ => Line::Other(line.to_string()),
    }
}

/// 実行ファイルの隣の ps1。`cargo run` で動かしたときのため、リポジトリの `tools/` も見る。
pub fn script_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    [
        dir.join(SCRIPT_NAME),
        dir.join("..").join("..").join("tools").join(SCRIPT_NAME),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

fn script_args(script: &Path, opts: &Options) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = vec![
        "-NoProfile".into(),
        "-NonInteractive".into(),
        // この 1 回の実行だけに効く。PC の実行ポリシーの設定は変えない
        "-ExecutionPolicy".into(),
        "Bypass".into(),
        "-File".into(),
        script.into(),
        "-Source".into(),
        opts.source.clone().into(),
        "-Output".into(),
        opts.output.clone().into(),
        "-Media".into(),
        opts.media.script_arg().into(),
        "-Machine".into(),
    ];
    if !opts.label.trim().is_empty() {
        args.push("-Label".into());
        args.push(opts.label.trim().into());
    }
    if opts.overwrite {
        args.push("-Force".into());
    }
    if opts.exclude_hidden {
        args.push("-ExcludeHidden".into());
    }
    args
}

pub fn create(opts: &Options, progress: &Progress) -> Result<Report, String> {
    let script = script_path().ok_or_else(|| {
        format!("{SCRIPT_NAME} が見つかりません。folder2iso.exe と同じフォルダに置いてください")
    })?;
    progress.set_stage("中身を数えています");
    let summary = iso::scan(&opts.source, opts.exclude_hidden)?;
    progress.total.store(summary.bytes, Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);

    let interpreter = std::env::var_os(INTERPRETER_ENV).unwrap_or_else(|| "powershell.exe".into());
    let mut cmd = Command::new(&interpreter);
    cmd.args(script_args(&script, opts))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // 画面から呼んだとき、PowerShell の黒い窓を出さない
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("PowerShell を起動できません: {e}"))?;

    let (tx, rx) = mpsc::channel();
    let stdout = child.stdout.take().expect("stdout is piped");
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).split(b'\n').map_while(Result::ok) {
            if tx.send(parse(&String::from_utf8_lossy(&line))).is_err() {
                break;
            }
        }
    });
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    });

    let mut warnings = vec![];
    let mut error = None;
    let mut other = vec![];
    let mut image_bytes = None;
    let mut writing = false;
    loop {
        if progress.cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            // 書き始める前に消すと、上書きのつもりで置いてあった前の ISO まで消してしまう
            if writing {
                let _ = std::fs::remove_file(&opts.output);
                return Err("中断しました（書きかけのファイルは消しました）".into());
            }
            return Err("中断しました".into());
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Line::Progress(done, total)) => {
                writing = true;
                progress.done.store(done, Ordering::Relaxed);
                progress.total.store(total, Ordering::Relaxed);
            }
            Ok(Line::Info(text)) => {
                writing |= text.starts_with(WRITING);
                progress.set_stage(text.trim_end_matches('.'));
            }
            Ok(Line::Warn(text)) => warnings.push(text),
            Ok(Line::Done(bytes)) => image_bytes = Some(bytes),
            Ok(Line::Error(text)) => error = Some(text),
            Ok(Line::Other(text)) => {
                if !text.trim().is_empty() {
                    other.push(text);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let status = child
        .wait()
        .map_err(|e| format!("PowerShell の終わりを待てません: {e}"))?;
    let stderr = stderr_reader.join().unwrap_or_default();
    if let Some(e) = error {
        return Err(e);
    }
    match image_bytes {
        Some(bytes) if status.success() => Ok(Report {
            summary,
            image_bytes: bytes,
            warnings,
        }),
        _ => {
            let detail: Vec<&str> = other
                .iter()
                .map(String::as_str)
                .chain(stderr.lines())
                .filter(|l| !l.trim().is_empty())
                .collect();
            Err(format!(
                "PowerShell が失敗しました（{status}）\n{}",
                detail.join("\n")
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iso::Media;

    /// PowerShell のある環境だけで動かす(`FOLDER2ISO_POWERSHELL=pwsh cargo test`)。
    /// Windows 以外では IMAPI2 がないので、スクリプトの失敗の理由がそのまま返ることを確かめる。
    #[test]
    fn runs_the_script_and_passes_its_progress_and_errors_back() {
        if std::env::var_os(INTERPRETER_ENV).is_none() {
            return;
        }
        let exe_dir = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let tools = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tools")
            .join(SCRIPT_NAME);
        std::fs::copy(&tools, exe_dir.join(SCRIPT_NAME)).unwrap();
        let src = std::env::temp_dir().join(format!("folder2iso-ps-{}", std::process::id()));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), b"a").unwrap();
        let opts = Options {
            source: src.clone(),
            output: src.with_extension("iso"),
            label: "ラベル".into(),
            overwrite: true,
            exclude_hidden: false,
            media: Media::Unlimited,
        };
        let progress = Progress::default();
        let result = create(&opts, &progress);
        let _ = std::fs::remove_dir_all(&src);
        if cfg!(windows) {
            let report = result.unwrap();
            assert!(report.image_bytes > 0);
            let _ = std::fs::remove_file(&opts.output);
        } else {
            let err = result.unwrap_err();
            assert!(err.contains("ComObject"), "{err}");
            // COM を作るところで止まるので、最後の経過は形式の行。日本語が化けずに届いている
            assert!(progress.stage().starts_with("形式"), "{}", progress.stage());
            assert!(progress.stage().contains("ラベル: ラベル"));
        }
    }

    #[test]
    fn machine_lines_are_parsed_by_kind() {
        assert_eq!(
            parse("@@INFO 中身を数えています...\r"),
            Line::Info("中身を数えています...".into())
        );
        assert_eq!(
            parse("@@WARN UDF だけで作ります"),
            Line::Warn("UDF だけで作ります".into())
        );
        assert_eq!(
            parse("@@PROGRESS 67108864 10291773440"),
            Line::Progress(67_108_864, 10_291_773_440)
        );
        assert_eq!(parse("@@DONE 10291773440"), Line::Done(10_291_773_440));
        assert_eq!(
            parse("@@ERROR 入りきりません"),
            Line::Error("入りきりません".into())
        );
    }

    #[test]
    fn other_output_is_kept_as_is_for_error_reports() {
        assert_eq!(
            parse("Add-Type : 型が見つかりません"),
            Line::Other("Add-Type : 型が見つかりません".into())
        );
        assert_eq!(
            parse("@@PROGRESS broken"),
            Line::Other("@@PROGRESS broken".into())
        );
    }

    #[test]
    fn the_script_gets_only_the_options_that_are_set() {
        let mut opts = Options {
            source: PathBuf::from("D:\\写真 2024"),
            output: PathBuf::from("E:\\out.iso"),
            label: String::new(),
            overwrite: false,
            exclude_hidden: false,
            media: Media::Bd,
        };
        let args = script_args(Path::new("C:\\tools\\folder2DVD_ISO.ps1"), &opts);
        let text: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(text.windows(2).any(|w| w == ["-Source", "D:\\写真 2024"]));
        assert!(text.windows(2).any(|w| w == ["-Media", "BD"]));
        assert!(text.contains(&"-Machine".to_string()));
        assert!(!text.contains(&"-Label".to_string()));
        assert!(!text.contains(&"-Force".to_string()));

        opts.label = " PHOTO ".into();
        opts.overwrite = true;
        opts.exclude_hidden = true;
        let text: Vec<String> = script_args(Path::new("x.ps1"), &opts)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(text.windows(2).any(|w| w == ["-Label", "PHOTO"]));
        assert!(text.contains(&"-Force".to_string()));
        assert!(text.contains(&"-ExcludeHidden".to_string()));
    }
}
