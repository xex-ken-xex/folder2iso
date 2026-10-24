//! フォルダから ISO9660 (Level 2) + Joliet のイメージを書き出す。
//!
//! レイアウト:
//!   LBA 0-15   システム領域（ゼロ埋め）
//!   LBA 16     Primary Volume Descriptor (ISO9660)
//!   LBA 17     Supplementary Volume Descriptor (Joliet)
//!   LBA 18     Volume Descriptor Set Terminator
//!   LBA 19-    パステーブル (ISO L/M, Joliet L/M)
//!   以降       ディレクトリ (ISO), ディレクトリ (Joliet), ファイル本体（両ツリーで共有）
//!
//! 既存のクレート（hadris-iso など）を使わないのは、書き出しの API が版ごとに変わっている途中で、
//! ここで要る形式（ISO9660 + Joliet、ファイルの中身を流しながら書く）が数百行で書けるため。
//! UDF は Joliet より仕様が大きく、4GiB 以上のファイルを入れたいとき以外は要らないので入れていない。

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const SECTOR: usize = 2048;
/// ISO9660 の 1 エクステントの上限 (4GiB-1)。複数エクステントは Windows が読めないことがあるので使わない。
pub const MAX_FILE: u64 = u32::MAX as u64;
const ISO_NAME_MAX: usize = 30; // Level 2
const JOLIET_NAME_MAX: usize = 64; // UTF-16 単位
/// ISO9660 の階層の上限。Joliet も同じ値を持つが、Windows はこれより深くても読める。
const DEPTH_LIMIT: usize = 8;

pub struct Options {
    pub source: PathBuf,
    pub output: PathBuf,
    /// 空ならフォルダ名
    pub label: String,
    pub overwrite: bool,
    pub exclude_hidden: bool,
    pub media: Media,
}

/// 焼く予定のメディア。入りきらなければ作る前に止める。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Media {
    Cd,
    Dvd,
    DvdDl,
    Bd,
    BdDl,
    /// 上限なし（焼かずにファイルとして使う）
    #[default]
    Unlimited,
}

impl Media {
    pub const ALL: [Media; 6] = [
        Media::Unlimited,
        Media::Cd,
        Media::Dvd,
        Media::DvdDl,
        Media::Bd,
        Media::BdDl,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Media::Cd => "CD (700MB)",
            Media::Dvd => "DVD (4.7GB)",
            Media::DvdDl => "DVD 2 層 (8.5GB)",
            Media::Bd => "BD (25GB)",
            Media::BdDl => "BD 2 層 (50GB)",
            Media::Unlimited => "上限なし",
        }
    }

    /// folder2DVD_ISO.ps1 の -Media に渡す値
    pub fn script_arg(self) -> &'static str {
        match self {
            Media::Cd => "CD",
            Media::Dvd => "DVD",
            Media::DvdDl => "DVD-DL",
            Media::Bd => "BD",
            Media::BdDl => "BD-DL",
            Media::Unlimited => "Unlimited",
        }
    }

    /// 入る量（2048 バイトのセクタ数）。IMAPI2 の ChooseImageDefaultsForMediaType と同じ値にそろえる。
    pub fn capacity_sectors(self) -> Option<u64> {
        match self {
            Media::Cd => Some(360_000),
            Media::Dvd => Some(2_295_104),
            Media::DvdDl => Some(4_173_824),
            Media::Bd => Some(12_219_392),
            Media::BdDl => Some(24_438_784),
            Media::Unlimited => None,
        }
    }
}

/// 書き出しの進み具合。別のスレッドから読み、中断を伝える。
#[derive(Default)]
pub struct Progress {
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub cancel: AtomicBool,
    /// いま何をしているか（書き出しが始まる前の段階を画面に出す）
    pub stage: Mutex<String>,
}

impl Progress {
    pub fn set_stage(&self, text: &str) {
        if let Ok(mut s) = self.stage.lock() {
            *s = text.to_string();
        }
    }

    pub fn stage(&self) -> String {
        self.stage.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

/// 入れるものの数と大きさ。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    pub files: usize,
    pub dirs: usize,
    pub bytes: u64,
    /// 4GiB 以上で入れられないファイル
    pub too_big: Vec<PathBuf>,
}

#[derive(Debug)]
pub struct Report {
    pub summary: Summary,
    pub image_bytes: u64,
    pub warnings: Vec<String>,
}

/// 出力の既定: 元のフォルダと同じ場所の `<フォルダ名>.iso`。ドライブ直下なら作業フォルダに。
pub fn default_output(source: &Path) -> PathBuf {
    let name = folder_name(source);
    match source.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join(format!("{name}.iso")),
        _ => PathBuf::from(format!("{name}.iso")),
    }
}

pub fn folder_name(source: &Path) -> String {
    source
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "CDROM".into())
}

/// 書き出さずに、入れるものを数える。
pub fn scan(source: &Path, exclude_hidden: bool) -> Result<Summary, String> {
    let img = Image::collect(source, exclude_hidden)?;
    Ok(img.summary())
}

pub fn create(opts: &Options, progress: &Progress) -> Result<Report, String> {
    let src = fs::canonicalize(&opts.source)
        .map_err(|e| format!("フォルダを開けません: {}: {e}", opts.source.display()))?;
    if !src.is_dir() {
        return Err(format!("フォルダではありません: {}", opts.source.display()));
    }
    let out_parent = opts
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if let Ok(p) = fs::canonicalize(out_parent) {
        if p.starts_with(&src) {
            return Err(
                "出力先を元のフォルダの中にはできません（自分自身を取り込んでしまうため）".into(),
            );
        }
    }
    let label = if opts.label.trim().is_empty() {
        folder_name(&src)
    } else {
        opts.label.trim().to_string()
    };

    let mut img = Image::collect(&src, opts.exclude_hidden)?;
    let summary = img.summary();
    if !summary.too_big.is_empty() {
        let list: Vec<String> = summary
            .too_big
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        return Err(format!(
            "4GiB 以上のファイルは ISO9660/Joliet に入れられません（UDF が要ります）:\n  {}",
            list.join("\n  ")
        ));
    }
    let layout = img.layout()?;
    if let Some(cap) = opts.media.capacity_sectors() {
        if layout.volume_sectors as u64 > cap {
            return Err(format!(
                "{} に入りきりません（イメージ {:.2} GB、容量 {:.2} GB）",
                opts.media.label(),
                layout.volume_sectors as f64 * SECTOR as f64 / 1e9,
                cap as f64 * SECTOR as f64 / 1e9
            ));
        }
    }
    progress.total.store(summary.bytes, Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);

    let mut open = OpenOptions::new();
    open.write(true);
    if opts.overwrite {
        open.create(true).truncate(true);
    } else {
        open.create_new(true);
    }
    let file = open.open(&opts.output).map_err(|e| {
        if e.kind() == io::ErrorKind::AlreadyExists {
            format!("出力ファイルが既にあります: {}", opts.output.display())
        } else {
            format!("出力ファイルを作れません: {}: {e}", opts.output.display())
        }
    })?;
    let mut w = Writer {
        w: BufWriter::with_capacity(1 << 20, file),
        pos: 0,
    };
    if let Err(e) = img.write(&mut w, &layout, &label, progress) {
        drop(w);
        let _ = fs::remove_file(&opts.output);
        return Err(if e.kind() == io::ErrorKind::Interrupted {
            "中断しました（書きかけのファイルは消しました）".into()
        } else {
            format!("書き込みに失敗しました: {e}")
        });
    }
    Ok(Report {
        summary,
        image_bytes: layout.volume_sectors as u64 * SECTOR as u64,
        warnings: img.warnings,
    })
}

struct Node {
    src: PathBuf,
    is_dir: bool,
    size: u64,
    mtime: SystemTime,
    parent: usize,
    children: Vec<usize>,
    /// ISO9660 の識別子（ファイルは ";1" 付き）
    iso_name: Vec<u8>,
    /// UCS-2 BE
    joliet_name: Vec<u8>,
    data_lba: u32,
    iso_lba: u32,
    iso_size: u32,
    jol_lba: u32,
    jol_size: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tree {
    Iso,
    Joliet,
}

struct Layout {
    iso_dirs: Vec<usize>,
    jol_dirs: Vec<usize>,
    files: Vec<usize>,
    /// ISO L, ISO M, Joliet L, Joliet M
    pt_lba: [u32; 4],
    pt_size: [u32; 2],
    volume_sectors: u32,
}

struct Image {
    nodes: Vec<Node>,
    warnings: Vec<String>,
}

impl Image {
    fn collect(src: &Path, exclude_hidden: bool) -> Result<Image, String> {
        let meta = fs::metadata(src)
            .map_err(|e| format!("フォルダを開けません: {}: {e}", src.display()))?;
        if !meta.is_dir() {
            return Err(format!("フォルダではありません: {}", src.display()));
        }
        let mut root = Node::new(src.to_path_buf(), true, 0, mtime_of(&meta), 0);
        root.iso_name = vec![0];
        root.joliet_name = vec![0];
        let mut img = Image {
            nodes: vec![root],
            warnings: vec![],
        };
        img.collect_dir(0, exclude_hidden, 1)?;
        Ok(img)
    }

    fn collect_dir(
        &mut self,
        dir: usize,
        exclude_hidden: bool,
        depth: usize,
    ) -> Result<(), String> {
        let path = self.nodes[dir].src.clone();
        let rd = fs::read_dir(&path)
            .map_err(|e| format!("フォルダを読めません: {}: {e}", path.display()))?;
        let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            let meta = match fs::symlink_metadata(&p) {
                Ok(m) => m,
                Err(err) => {
                    self.warnings
                        .push(format!("読めないので飛ばした: {}: {err}", p.display()));
                    continue;
                }
            };
            // リンク先をたどらないのは、ジャンクションがループしていると終わらないため
            if meta.file_type().is_symlink() {
                self.warnings.push(format!(
                    "シンボリックリンク・ジャンクションは飛ばした: {}",
                    p.display()
                ));
                continue;
            }
            if exclude_hidden && is_hidden(&name, &meta) {
                continue;
            }
            let is_dir = meta.is_dir();
            if !is_dir && !meta.is_file() {
                continue;
            }
            let idx = self.nodes.len();
            let size = if is_dir { 0 } else { meta.len() };
            self.nodes
                .push(Node::new(p.clone(), is_dir, size, mtime_of(&meta), dir));
            self.nodes[dir].children.push(idx);
            if is_dir {
                if depth == DEPTH_LIMIT {
                    self.warnings.push(format!(
                        "ISO9660 の階層の上限（{DEPTH_LIMIT}）より深い（Windows では読めます）: {}",
                        p.display()
                    ));
                }
                self.collect_dir(idx, exclude_hidden, depth + 1)?;
            }
        }
        self.assign_names(dir);
        Ok(())
    }

    fn summary(&self) -> Summary {
        let files = self.nodes.iter().filter(|n| !n.is_dir);
        Summary {
            files: files.clone().count(),
            dirs: self.nodes.len() - 1 - files.clone().count(),
            bytes: files.clone().map(|n| n.size).sum(),
            too_big: files
                .filter(|n| n.size > MAX_FILE)
                .map(|n| n.src.clone())
                .collect(),
        }
    }

    /// 1 つのフォルダの子に ISO9660 名と Joliet 名を付け、重なりを解く。
    fn assign_names(&mut self, dir: usize) {
        let mut used_iso = HashSet::new();
        let mut used_jol = HashSet::new();
        for c in self.nodes[dir].children.clone() {
            let orig = self.nodes[c]
                .src
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let is_dir = self.nodes[c].is_dir;
            let iso = (0..)
                .map(|n| iso_name(&orig, is_dir, n))
                .find(|id| used_iso.insert(id.clone()))
                .unwrap_or_default();
            // Windows は大文字小文字を区別しないので、区別せずに重なりを見る
            let jol = (0..)
                .map(|n| joliet_name(&orig, is_dir, n))
                .find(|id| used_jol.insert(id.to_lowercase()))
                .unwrap_or_default();
            if jol != orig {
                self.warnings.push(format!(
                    "名前を変えて入れた: {} -> {jol}",
                    self.nodes[c].src.display()
                ));
            }
            self.nodes[c].iso_name = iso.into_bytes();
            self.nodes[c].joliet_name = jol.encode_utf16().flat_map(u16::to_be_bytes).collect();
        }
    }

    fn name(&self, i: usize, t: Tree) -> &[u8] {
        match t {
            Tree::Iso => &self.nodes[i].iso_name,
            Tree::Joliet => &self.nodes[i].joliet_name,
        }
    }

    fn sorted_children(&self, i: usize, t: Tree) -> Vec<usize> {
        let mut c = self.nodes[i].children.clone();
        c.sort_by(|&a, &b| self.name(a, t).cmp(self.name(b, t)));
        c
    }

    /// パステーブルの順（階層順、その中は親の番号順、さらに名前順）のフォルダ。
    fn dir_order(&self, t: Tree) -> Vec<usize> {
        let mut order = vec![0usize];
        let mut k = 0;
        while k < order.len() {
            let d = order[k];
            order.extend(
                self.sorted_children(d, t)
                    .into_iter()
                    .filter(|&c| self.nodes[c].is_dir),
            );
            k += 1;
        }
        order
    }

    fn dir_size(&self, d: usize, t: Tree) -> u32 {
        let lens = [rec_len(1), rec_len(1)].into_iter().chain(
            self.nodes[d]
                .children
                .iter()
                .map(|&c| rec_len(self.name(c, t).len())),
        );
        let (mut secs, mut off) = (1u32, 0usize);
        for l in lens {
            if off + l > SECTOR {
                secs += 1;
                off = 0;
            }
            off += l;
        }
        secs * SECTOR as u32
    }

    fn path_table_size(&self, dirs: &[usize], t: Tree) -> u32 {
        dirs.iter()
            .map(|&d| {
                let l = self.name(d, t).len();
                (8 + l + l % 2) as u32
            })
            .sum()
    }

    fn layout(&mut self) -> Result<Layout, String> {
        let iso_dirs = self.dir_order(Tree::Iso);
        let jol_dirs = self.dir_order(Tree::Joliet);
        let pt_size = [
            self.path_table_size(&iso_dirs, Tree::Iso),
            self.path_table_size(&jol_dirs, Tree::Joliet),
        ];
        let mut lba: u64 = 19;
        let mut pt_lba = [0u32; 4];
        for (i, slot) in pt_lba.iter_mut().enumerate() {
            *slot = lba as u32;
            lba += sectors(pt_size[i / 2] as u64);
        }
        for &d in &iso_dirs {
            let size = self.dir_size(d, Tree::Iso);
            self.nodes[d].iso_lba = lba as u32;
            self.nodes[d].iso_size = size;
            lba += (size as usize / SECTOR) as u64;
        }
        for &d in &jol_dirs {
            let size = self.dir_size(d, Tree::Joliet);
            self.nodes[d].jol_lba = lba as u32;
            self.nodes[d].jol_size = size;
            lba += (size as usize / SECTOR) as u64;
        }
        let mut files = vec![];
        for &d in &iso_dirs {
            files.extend(
                self.sorted_children(d, Tree::Iso)
                    .into_iter()
                    .filter(|&c| !self.nodes[c].is_dir),
            );
        }
        for &f in &files {
            self.nodes[f].data_lba = lba as u32;
            lba += sectors(self.nodes[f].size);
            if lba > u32::MAX as u64 {
                return Err("イメージ全体が大きすぎます（8TiB 超）".into());
            }
        }
        Ok(Layout {
            iso_dirs,
            jol_dirs,
            files,
            pt_lba,
            pt_size,
            volume_sectors: lba as u32,
        })
    }

    fn write(
        &mut self,
        w: &mut Writer,
        l: &Layout,
        label: &str,
        progress: &Progress,
    ) -> io::Result<()> {
        let now = SystemTime::now();
        w.put(&[0u8; 16 * SECTOR])?;
        w.put(&self.volume_descriptor(Tree::Iso, label, l, now))?;
        w.put(&self.volume_descriptor(Tree::Joliet, label, l, now))?;
        let mut term = [0u8; SECTOR];
        term[0] = 255;
        term[1..6].copy_from_slice(b"CD001");
        term[6] = 1;
        w.put(&term)?;

        let tables = [
            (&l.iso_dirs, Tree::Iso, false),
            (&l.iso_dirs, Tree::Iso, true),
            (&l.jol_dirs, Tree::Joliet, false),
            (&l.jol_dirs, Tree::Joliet, true),
        ];
        for (i, (dirs, t, big_endian)) in tables.into_iter().enumerate() {
            w.expect_lba(l.pt_lba[i])?;
            w.put(&self.path_table(dirs, t, big_endian))?;
            w.pad_sector()?;
        }
        for &d in &l.iso_dirs {
            self.write_dir(w, d, Tree::Iso)?;
        }
        for &d in &l.jol_dirs {
            self.write_dir(w, d, Tree::Joliet)?;
        }

        let mut buf = vec![0u8; 1 << 20];
        for &f in &l.files {
            let (src, size, lba) = {
                let n = &self.nodes[f];
                (n.src.clone(), n.size, n.data_lba)
            };
            w.expect_lba(lba)?;
            let mut fh = File::open(&src)
                .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", src.display())))?;
            let mut remaining = size;
            while remaining > 0 {
                if progress.cancel.load(Ordering::Relaxed) {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                let want = remaining.min(buf.len() as u64) as usize;
                let mut got = fh.read(&mut buf[..want])?;
                if got == 0 {
                    // 数えたあとに縮んだファイルは、並べた位置がずれないようゼロで埋める
                    self.warnings.push(format!(
                        "書いている間に小さくなったので、足りない分をゼロで埋めた: {}",
                        src.display()
                    ));
                    buf[..want].fill(0);
                    got = want;
                }
                w.put(&buf[..got])?;
                remaining -= got as u64;
                progress.done.fetch_add(got as u64, Ordering::Relaxed);
            }
            w.pad_sector()?;
        }
        w.expect_lba(l.volume_sectors)?;
        w.w.flush()?;
        w.w.get_ref().sync_all()
    }

    fn volume_descriptor(&self, t: Tree, label: &str, l: &Layout, now: SystemTime) -> Vec<u8> {
        let (ty, pt_size, l_lba, m_lba) = match t {
            Tree::Iso => (1, l.pt_size[0], l.pt_lba[0], l.pt_lba[1]),
            Tree::Joliet => (2, l.pt_size[1], l.pt_lba[2], l.pt_lba[3]),
        };
        let mut v = vec![0u8; SECTOR];
        v[0] = ty;
        v[1..6].copy_from_slice(b"CD001");
        v[6] = 1;
        let iso_label = iso_label(label);
        let label = if t == Tree::Iso {
            iso_label.as_str()
        } else {
            label
        };
        fill_field(&mut v[8..40], "", t); // system id
        fill_field(&mut v[40..72], label, t);
        v[80..88].copy_from_slice(&both32(l.volume_sectors));
        if t == Tree::Joliet {
            v[88..91].copy_from_slice(b"%/E"); // UCS-2 Level 3
        }
        v[120..124].copy_from_slice(&both16(1));
        v[124..128].copy_from_slice(&both16(1));
        v[128..132].copy_from_slice(&both16(SECTOR as u16));
        v[132..140].copy_from_slice(&both32(pt_size));
        v[140..144].copy_from_slice(&l_lba.to_le_bytes());
        v[148..152].copy_from_slice(&m_lba.to_be_bytes());
        let root = &self.nodes[0];
        let (lba, size) = root.extent(t);
        v[156..190].copy_from_slice(&dir_record(lba, size, root.mtime, true, &[0]));
        fill_field(&mut v[190..318], label, t); // volume set id
        fill_field(&mut v[318..446], "", t); // publisher
        fill_field(&mut v[446..574], "", t); // data preparer
        fill_field(&mut v[574..702], "FOLDER2ISO", t); // application
        fill_field(&mut v[702..813], "", t); // copyright / abstract / bibliographic
        v[813..830].copy_from_slice(&date17(now));
        v[830..847].copy_from_slice(&date17(now));
        v[847..863].copy_from_slice(b"0000000000000000");
        v[864..880].copy_from_slice(b"0000000000000000");
        v[881] = 1;
        v
    }

    fn path_table(&self, dirs: &[usize], t: Tree, big_endian: bool) -> Vec<u8> {
        let number: HashMap<usize, u16> = dirs
            .iter()
            .enumerate()
            .map(|(i, &d)| (d, (i + 1) as u16))
            .collect();
        let mut out = vec![];
        for &d in dirs {
            let name = self.name(d, t);
            let node = &self.nodes[d];
            let lba = node.extent(t).0;
            let parent = number[&node.parent];
            out.push(name.len() as u8);
            out.push(0);
            if big_endian {
                out.extend_from_slice(&lba.to_be_bytes());
                out.extend_from_slice(&parent.to_be_bytes());
            } else {
                out.extend_from_slice(&lba.to_le_bytes());
                out.extend_from_slice(&parent.to_le_bytes());
            }
            out.extend_from_slice(name);
            if name.len() % 2 == 1 {
                out.push(0);
            }
        }
        out
    }

    fn write_dir(&self, w: &mut Writer, d: usize, t: Tree) -> io::Result<()> {
        let node = &self.nodes[d];
        let parent = &self.nodes[node.parent];
        let (lba, size) = node.extent(t);
        let (plba, psize) = parent.extent(t);
        w.expect_lba(lba)?;
        let mut records = vec![
            dir_record(lba, size, node.mtime, true, &[0]),
            dir_record(plba, psize, parent.mtime, true, &[1]),
        ];
        for c in self.sorted_children(d, t) {
            let n = &self.nodes[c];
            let (l, s) = n.extent(t);
            records.push(dir_record(l, s, n.mtime, n.is_dir, self.name(c, t)));
        }
        // 1 つのレコードはセクタをまたげない
        let start = w.pos;
        for r in records {
            let off = ((w.pos - start) % SECTOR as u64) as usize;
            if off + r.len() > SECTOR {
                w.pad_sector()?;
            }
            w.put(&r)?;
        }
        w.pad_sector()
    }
}

impl Node {
    fn new(src: PathBuf, is_dir: bool, size: u64, mtime: SystemTime, parent: usize) -> Node {
        Node {
            src,
            is_dir,
            size,
            mtime,
            parent,
            children: vec![],
            iso_name: vec![],
            joliet_name: vec![],
            data_lba: 0,
            iso_lba: 0,
            iso_size: 0,
            jol_lba: 0,
            jol_size: 0,
        }
    }

    /// ディレクトリレコードに書く位置と長さ。
    fn extent(&self, t: Tree) -> (u32, u32) {
        match (self.is_dir, t) {
            (true, Tree::Iso) => (self.iso_lba, self.iso_size),
            (true, Tree::Joliet) => (self.jol_lba, self.jol_size),
            (false, _) => (self.data_lba, self.size as u32),
        }
    }
}

fn mtime_of(m: &fs::Metadata) -> SystemTime {
    m.modified().unwrap_or(UNIX_EPOCH)
}

fn is_hidden(name: &str, _meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
        if _meta.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0 {
            return true;
        }
    }
    name.starts_with('.')
}

// ---- 名前 ----

/// ISO9660 Level 2 の名前: A-Z 0-9 _ だけ、30 文字まで。`n` が 1 以上なら重なりを避ける番号を付ける。
fn iso_name(orig: &str, is_dir: bool, n: usize) -> String {
    let (base, ext) = split_ext(orig, is_dir);
    let base = to_dchars(&base);
    let ext: String = to_dchars(&ext).chars().take(8).collect();
    let max_base = if is_dir {
        ISO_NAME_MAX
    } else {
        ISO_NAME_MAX - 1 - ext.len()
    };
    let b = with_suffix(&base, max_base, n);
    if is_dir {
        b
    } else {
        format!("{b}.{ext};1")
    }
}

/// Joliet の名前: 禁止の字 `* / : ; ? \` と制御文字を `_` にし、UTF-16 で 64 単位まで。
/// 版番号 ";1" を付けないのは、Windows も Linux も付けなくて読め、付けると名前の上限が 2 字減るため。
fn joliet_name(orig: &str, is_dir: bool, n: usize) -> String {
    let clean: String = orig
        .chars()
        .map(|ch| {
            if "*/:;?\\".contains(ch) || (ch as u32) < 0x20 {
                '_'
            } else {
                ch
            }
        })
        .collect();
    let (base, ext) = split_ext(&clean, is_dir);
    // 拡張子は、長すぎるときだけ半分まで削る（名前の頭が残るほうが見分けやすい）
    let ext = truncate_utf16(&ext, JOLIET_NAME_MAX / 2);
    let ext_units = if ext.is_empty() {
        0
    } else {
        ext.encode_utf16().count() + 1
    };
    let suffix = if n == 0 {
        String::new()
    } else {
        format!(" ({n})")
    };
    let budget = JOLIET_NAME_MAX - ext_units - suffix.len();
    let base = if base.is_empty() {
        "_".to_string()
    } else {
        base
    };
    let b = format!("{}{suffix}", truncate_utf16(&base, budget));
    if ext.is_empty() {
        b
    } else {
        format!("{b}.{ext}")
    }
}

/// ISO9660 側のラベル。日本語だけのラベルは全部 `_` になって読めないので、そのときは "CDROM" にする
/// (Windows のエクスプローラは Joliet 側のラベルを出すので、ふだん目に入るのはそちら)。
fn iso_label(label: &str) -> String {
    let l: String = to_dchars(label).chars().take(32).collect();
    if l.chars().all(|c| c == '_') {
        "CDROM".into()
    } else {
        l
    }
}

fn split_ext(name: &str, is_dir: bool) -> (String, String) {
    match name.rfind('.') {
        Some(i) if !is_dir && i > 0 => (name[..i].to_string(), name[i + 1..].to_string()),
        _ => (name.to_string(), String::new()),
    }
}

fn to_dchars(s: &str) -> String {
    s.chars()
        .map(|c| {
            let c = c.to_ascii_uppercase();
            if c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn with_suffix(base: &str, max: usize, n: usize) -> String {
    let base = if base.is_empty() { "_" } else { base };
    if n == 0 {
        return base.chars().take(max).collect();
    }
    let suffix = format!("_{n}");
    let keep = max.saturating_sub(suffix.len());
    format!("{}{suffix}", base.chars().take(keep).collect::<String>())
}

/// サロゲートペアを割らずに、UTF-16 で `max_units` 単位までに切る。
fn truncate_utf16(s: &str, max_units: usize) -> String {
    let mut units = 0;
    s.chars()
        .take_while(|ch| {
            units += ch.len_utf16();
            units <= max_units
        })
        .collect()
}

// ---- 低レベルの書き込み ----

struct Writer {
    w: BufWriter<File>,
    pos: u64,
}

impl Writer {
    fn put(&mut self, b: &[u8]) -> io::Result<()> {
        self.pos += b.len() as u64;
        self.w.write_all(b)
    }

    fn pad_sector(&mut self) -> io::Result<()> {
        let r = (self.pos % SECTOR as u64) as usize;
        if r != 0 {
            self.put(&vec![0u8; SECTOR - r])?;
        }
        Ok(())
    }

    /// 先に決めた配置と、実際に書いている位置が合っているか。合わなければ壊れたイメージになるので止める。
    fn expect_lba(&self, lba: u32) -> io::Result<()> {
        if self.pos == lba as u64 * SECTOR as u64 {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "内部エラー: 配置がずれた（{} != {}）",
                self.pos,
                lba as u64 * SECTOR as u64
            )))
        }
    }
}

fn sectors(bytes: u64) -> u64 {
    bytes.div_ceil(SECTOR as u64)
}

fn rec_len(name_len: usize) -> usize {
    33 + name_len + (1 - name_len % 2)
}

fn both16(v: u16) -> [u8; 4] {
    let (l, b) = (v.to_le_bytes(), v.to_be_bytes());
    [l[0], l[1], b[0], b[1]]
}

fn both32(v: u32) -> [u8; 8] {
    let (l, b) = (v.to_le_bytes(), v.to_be_bytes());
    [l[0], l[1], l[2], l[3], b[0], b[1], b[2], b[3]]
}

/// UTC の (年, 月, 日, 時, 分, 秒)。
fn civil(t: SystemTime) -> (i64, u32, u32, u32, u32, u32) {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    // Howard Hinnant の civil_from_days
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        (rem % 3600 / 60) as u32,
        (rem % 60) as u32,
    )
}

fn date7(t: SystemTime) -> [u8; 7] {
    let (y, mo, d, h, mi, s) = civil(t);
    let y = (y - 1900).clamp(0, 255) as u8;
    [y, mo as u8, d as u8, h as u8, mi as u8, s as u8, 0]
}

fn date17(t: SystemTime) -> [u8; 17] {
    let (y, mo, d, h, mi, s) = civil(t);
    let text = format!("{:04}{mo:02}{d:02}{h:02}{mi:02}{s:02}00", y.clamp(0, 9999));
    let mut out = [0u8; 17];
    out[..16].copy_from_slice(text.as_bytes());
    out
}

fn dir_record(lba: u32, size: u32, mtime: SystemTime, is_dir: bool, name: &[u8]) -> Vec<u8> {
    let len = rec_len(name.len());
    let mut r = Vec::with_capacity(len);
    r.push(len as u8);
    r.push(0);
    r.extend_from_slice(&both32(lba));
    r.extend_from_slice(&both32(size));
    r.extend_from_slice(&date7(mtime));
    r.push(if is_dir { 2 } else { 0 });
    r.extend_from_slice(&[0, 0]);
    r.extend_from_slice(&both16(1));
    r.push(name.len() as u8);
    r.extend_from_slice(name);
    if name.len().is_multiple_of(2) {
        r.push(0);
    }
    r
}

/// 文字の欄を埋める。ISO9660 は空白、Joliet は UCS-2 の空白 (00 20) で残りを埋める。
fn fill_field(buf: &mut [u8], s: &str, t: Tree) {
    match t {
        Tree::Iso => {
            buf.fill(b' ');
            for (dst, b) in buf.iter_mut().zip(s.bytes()) {
                *dst = b;
            }
        }
        Tree::Joliet => {
            for (i, b) in buf.iter_mut().enumerate() {
                *b = if i % 2 == 0 { 0x00 } else { 0x20 };
            }
            let units = truncate_utf16(s, buf.len() / 2);
            for (pair, u) in buf
                .as_chunks_mut::<2>()
                .0
                .iter_mut()
                .zip(units.encode_utf16())
            {
                pair.copy_from_slice(&u.to_be_bytes());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("folder2iso-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn iso_names_are_uppercase_dchars_with_version() {
        assert_eq!(iso_name("readme.txt", false, 0), "README.TXT;1");
        assert_eq!(iso_name("写真 2024.jpeg", false, 0), "___2024.JPEG;1");
        assert_eq!(iso_name("Makefile", false, 0), "MAKEFILE.;1");
        assert_eq!(iso_name("my.dir", true, 0), "MY_DIR");
    }

    #[test]
    fn iso_names_fit_level2_and_collisions_get_a_number() {
        let long = "a".repeat(50) + ".txt";
        let first = iso_name(&long, false, 0);
        let second = iso_name(&long, false, 1);
        assert_eq!(first.trim_end_matches(";1").len(), 30);
        assert_ne!(first, second);
        assert!(second.contains("_1.TXT"));
    }

    #[test]
    fn joliet_names_keep_unicode_and_replace_forbidden_chars() {
        assert_eq!(
            joliet_name("日本語のファイル.txt", false, 0),
            "日本語のファイル.txt"
        );
        assert_eq!(joliet_name("a;b?c.txt", false, 0), "a_b_c.txt");
        assert_eq!(joliet_name("x.txt", false, 2), "x (2).txt");
    }

    #[test]
    fn long_joliet_names_are_cut_to_64_units_keeping_the_extension() {
        let name = "あ".repeat(100) + ".docx";
        let j = joliet_name(&name, false, 0);
        assert_eq!(j.encode_utf16().count(), 64);
        assert!(j.ends_with(".docx"));
    }

    #[test]
    fn a_label_without_latin_letters_falls_back_to_cdrom_on_the_iso_side() {
        assert_eq!(iso_label("テスト"), "CDROM");
        assert_eq!(iso_label("Backup 2024"), "BACKUP_2024");
    }

    #[test]
    fn truncation_does_not_split_surrogate_pairs() {
        assert_eq!(truncate_utf16("a😀", 2), "a");
        assert_eq!(truncate_utf16("a😀", 3), "a😀");
    }

    #[test]
    fn dates_are_utc_calendar_fields() {
        let t = UNIX_EPOCH + Duration::from_secs(1_709_210_096); // 2024-02-29 12:34:56 UTC
        assert_eq!(civil(t), (2024, 2, 29, 12, 34, 56));
        assert_eq!(&date17(t)[..16], b"2024022912345600");
        assert_eq!(date7(t), [124, 2, 29, 12, 34, 56, 0]);
    }

    #[test]
    fn records_never_straddle_a_sector() {
        let mut img = Image {
            nodes: vec![Node::new("/".into(), true, 0, UNIX_EPOCH, 0)],
            warnings: vec![],
        };
        img.nodes[0].iso_name = vec![0];
        for i in 0..100 {
            let mut n = Node::new(format!("/{i}").into(), false, 0, UNIX_EPOCH, 0);
            n.iso_name = format!("FILE_NAME_{i:04}.TXT;1").into_bytes(); // 19 字 → 52 バイト
            img.nodes.push(n);
            img.nodes[0].children.push(i + 1);
        }
        // 2048 / 52 = 39 個で 1 セクタ。"." ".." と 100 個で 102 個 → 3 セクタ
        assert_eq!(img.dir_size(0, Tree::Iso), 3 * SECTOR as u32);
    }

    #[test]
    fn creates_an_image_with_both_volume_descriptors_and_the_file_data() {
        let src = temp_dir("src");
        fs::create_dir_all(src.join("サブ")).unwrap();
        fs::write(src.join("hello.txt"), b"hello iso").unwrap();
        fs::write(src.join("サブ").join("データ.bin"), vec![7u8; 5000]).unwrap();
        let out = temp_dir("out").join("t.iso");
        let report = create(
            &Options {
                source: src.clone(),
                output: out.clone(),
                label: "テスト".into(),
                overwrite: false,
                exclude_hidden: false,
                media: Media::Unlimited,
            },
            &Progress::default(),
        )
        .unwrap();
        assert_eq!(report.summary.files, 2);
        assert_eq!(report.summary.dirs, 1);
        let iso = fs::read(&out).unwrap();
        assert_eq!(iso.len() as u64, report.image_bytes);
        assert_eq!(iso.len() % SECTOR, 0);
        let pvd = &iso[16 * SECTOR..17 * SECTOR];
        let svd = &iso[17 * SECTOR..18 * SECTOR];
        assert_eq!(&pvd[..7], b"\x01CD001\x01");
        assert_eq!(&svd[..7], b"\x02CD001\x01");
        assert_eq!(&svd[88..91], b"%/E");
        assert_eq!(&iso[18 * SECTOR..18 * SECTOR + 7], b"\xffCD001\x01");
        let label: Vec<u8> = "テスト".encode_utf16().flat_map(u16::to_be_bytes).collect();
        assert_eq!(&svd[40..40 + label.len()], &label[..]);
        assert!(iso.windows(9).any(|w| w == b"hello iso"));
        let _ = fs::remove_dir_all(&src);
    }

    #[test]
    fn refuses_to_overwrite_unless_asked() {
        let src = temp_dir("src2");
        let out = temp_dir("out2").join("t.iso");
        fs::write(&out, b"keep").unwrap();
        let mut opts = Options {
            source: src.clone(),
            output: out.clone(),
            label: String::new(),
            overwrite: false,
            exclude_hidden: false,
            media: Media::Unlimited,
        };
        assert!(create(&opts, &Progress::default()).is_err());
        assert_eq!(fs::read(&out).unwrap(), b"keep");
        opts.overwrite = true;
        assert!(create(&opts, &Progress::default()).is_ok());
    }

    #[test]
    fn refuses_an_output_inside_the_source() {
        let src = temp_dir("src3");
        let opts = Options {
            source: src.clone(),
            output: src.join("x.iso"),
            label: String::new(),
            overwrite: false,
            exclude_hidden: false,
            media: Media::Unlimited,
        };
        assert!(create(&opts, &Progress::default()).is_err());
        assert!(!src.join("x.iso").exists());
    }

    #[test]
    fn cancelling_removes_the_partial_image() {
        let src = temp_dir("src4");
        fs::write(src.join("a.bin"), vec![1u8; 10_000]).unwrap();
        let out = temp_dir("out4").join("t.iso");
        let progress = Progress::default();
        progress.cancel.store(true, Ordering::Relaxed);
        let opts = Options {
            source: src,
            output: out.clone(),
            label: String::new(),
            overwrite: false,
            exclude_hidden: false,
            media: Media::Unlimited,
        };
        assert!(create(&opts, &progress).unwrap_err().contains("中断"));
        assert!(!out.exists());
    }

    #[test]
    fn refuses_an_image_larger_than_the_chosen_media() {
        let src = temp_dir("src5");
        // 中身を書かずに大きさだけ決める（容量の判定は書き始める前なので、中身は読まれない）
        File::create(src.join("a.bin"))
            .unwrap()
            .set_len(800_000_000)
            .unwrap();
        let out = temp_dir("out5").join("t.iso");
        let opts = Options {
            source: src.clone(),
            output: out.clone(),
            label: String::new(),
            overwrite: false,
            exclude_hidden: false,
            media: Media::Cd,
        };
        assert!(create(&opts, &Progress::default())
            .unwrap_err()
            .contains("入りきりません"));
        assert!(!out.exists());
        let _ = fs::remove_dir_all(&src);
    }

    #[test]
    fn default_output_sits_next_to_the_source_folder() {
        assert_eq!(
            default_output(Path::new("/data/photos")),
            PathBuf::from("/data/photos.iso")
        );
    }
}
