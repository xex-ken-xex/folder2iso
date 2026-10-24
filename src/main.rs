//! フォルダから ISO9660 + Joliet の ISO イメージを作る。
//! 引数なしで起動すると画面(GUI)が開く。引数があればコマンドラインで動く。
//!
//! Windows では GUI サブシステムで作る(ダブルクリックで黒いコンソールの窓が出ないように)。
//! コマンドラインで使うときは、起動元のコンソールにつなぎ直して出力する(`attach_console`)。
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(feature = "gui")]
mod about;
#[cfg(feature = "gui")]
mod fonts;
#[cfg(feature = "gui")]
mod gui;
mod imapi;
mod iso;

use clap::Parser;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(
    version,
    about = "フォルダから ISO9660 + Joliet の ISO イメージを作る(引数なしで画面が開く)"
)]
struct Args {
    /// ISO に入れるフォルダ
    source: Option<PathBuf>,
    /// 出力する ISO ファイル(省略すると、フォルダと同じ場所の <フォルダ名>.iso)
    output: Option<PathBuf>,
    /// ボリュームラベル(省略するとフォルダ名)
    #[arg(short = 'V', long)]
    label: Option<String>,
    /// 出力ファイルが既にあれば上書きする
    #[arg(short, long)]
    force: bool,
    /// 隠しファイル・フォルダ(Windows の隠し属性か、名前が "." で始まるもの)を入れない
    #[arg(long)]
    exclude_hidden: bool,
    /// 焼く予定のメディア。入りきらなければ作る前に止める
    #[arg(long, value_enum, default_value_t = iso::Media::Unlimited)]
    media: iso::Media,
    /// 画面を開く(元のフォルダを指定すると、それを入れた状態で開く)
    #[arg(long)]
    gui: bool,
}

#[cfg(windows)]
fn attach_console() {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    // SAFETY: 引数なしの Win32 API。親にコンソールがなければ失敗して返るだけ
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(not(windows))]
fn attach_console() {}

fn main() {
    if std::env::args().len() > 1 {
        attach_console();
    }
    let args = Args::parse();
    if args.gui || args.source.is_none() {
        open_gui(args);
        return;
    }
    if let Err(e) = run_cli(args) {
        eprintln!("エラー: {e}");
        std::process::exit(1);
    }
}

#[cfg(feature = "gui")]
fn open_gui(args: Args) {
    if let Err(e) = gui::run(args.source) {
        eprintln!("エラー: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(feature = "gui"))]
fn open_gui(_args: Args) {
    use clap::CommandFactory;
    let _ = Args::command().print_help();
    std::process::exit(2);
}

/// ISO を作る。Windows では IMAPI2 (同じフォルダの folder2DVD_ISO.ps1)、ほかでは自前の書き出し。
pub fn build(opts: &iso::Options, progress: &iso::Progress) -> Result<iso::Report, String> {
    if cfg!(windows) {
        imapi::create(opts, progress)
    } else {
        iso::create(opts, progress)
    }
}

fn run_cli(args: Args) -> Result<(), String> {
    let source = args.source.expect("source is checked by main");
    let output = args.output.unwrap_or_else(|| iso::default_output(&source));
    let opts = iso::Options {
        source,
        output,
        label: args.label.unwrap_or_default(),
        overwrite: args.force,
        exclude_hidden: args.exclude_hidden,
        media: args.media,
    };
    let progress = Arc::new(iso::Progress::default());
    let started = Instant::now();
    let shown = Arc::clone(&progress);
    let ticker = std::thread::spawn(move || {
        let mut last = u64::MAX;
        while !shown.cancel.load(Ordering::Relaxed) {
            let (done, total) = (
                shown.done.load(Ordering::Relaxed),
                shown.total.load(Ordering::Relaxed),
            );
            let pct = (done * 100).checked_div(total).unwrap_or(0);
            if total > 0 && pct != last {
                eprint!("\r書き込み中... {pct:3}%");
                last = pct;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    });
    let result = build(&opts, &progress);
    // 進み具合の表示を止める合図に使う。書き出しはもう終わっているので中断にはならない
    progress.cancel.store(true, Ordering::Relaxed);
    let _ = ticker.join();
    eprint!("\r                        \r");
    let report = result?;
    for w in &report.warnings {
        eprintln!("警告: {w}");
    }
    println!(
        "作成しました: {}  ({} ファイル, {} フォルダ, {:.1} MiB, {:.1} 秒)",
        opts.output.display(),
        report.summary.files,
        report.summary.dirs,
        report.image_bytes as f64 / 1048576.0,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
