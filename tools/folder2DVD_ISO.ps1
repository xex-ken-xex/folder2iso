<#
.SYNOPSIS
    フォルダから ISO イメージを作る（Windows 標準の IMAPI2 を使う）。

.DESCRIPTION
    指定したフォルダの中身をまるごと入れた .iso ファイルを作る。
    Windows に標準で入っている IMAPI2（imapi2fs.dll）にイメージを作らせるので、追加のソフトは要らない。
    UDF に対応しているので、4GiB 以上のファイルも入れられる（そのときは自動で UDF だけで作る）。
    folder2iso.exe も、Windows ではこのスクリプトを呼んで ISO を作る。

    詳しくは同じフォルダの README.md を参照。

.PARAMETER Source
    ISO に入れるフォルダ。

.PARAMETER Output
    出力する .iso ファイル。省略すると、元のフォルダと同じ場所の「フォルダ名.iso」。

.PARAMETER Label
    ボリュームラベル（ドライブ名の横に出る名前）。省略するとフォルダ名。

.PARAMETER Media
    焼く予定のメディア。入りきらないときは作る前に止める。
    CD / DVD（既定、片面 1 層 4.7GB）/ DVD-DL（2 層 8.5GB）/ BD（25GB）/ BD-DL（50GB）/ Unlimited（上限なし）

.PARAMETER FileSystem
    Auto（既定）: ISO9660 + Joliet + UDF。4GiB 以上のファイルがあれば UDF だけ
    All: ISO9660 + Joliet + UDF（4GiB 以上のファイルがあると失敗する）
    UDF: UDF だけ

.PARAMETER ExcludeHidden
    隠しファイル・隠しフォルダ（隠し属性かシステム属性のもの、名前が "." で始まるもの）を入れない。

.PARAMETER Force
    出力ファイルが既にあれば上書きする。

.PARAMETER Machine
    folder2iso.exe から呼ぶときの出力にする（1 行ずつ "@@種類 内容" を UTF-8 で出す）。手で使うときは要らない。

.EXAMPLE
    .\folder2DVD_ISO.ps1 "D:\写真\2024"

.EXAMPLE
    .\folder2DVD_ISO.ps1 -Source "D:\写真\2024" -Output "E:\photo2024.iso" -Label "PHOTO2024" -Media BD -Force
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [string]$Source,

    [Parameter(Position = 1)]
    [string]$Output,

    [string]$Label,

    [ValidateSet('CD', 'DVD', 'DVD-DL', 'BD', 'BD-DL', 'Unlimited')]
    [string]$Media = 'DVD',

    [ValidateSet('Auto', 'All', 'UDF')]
    [string]$FileSystem = 'Auto',

    [switch]$ExcludeHidden,

    [switch]$Force,

    [switch]$Machine
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'

if ($Machine) {
    # 呼び出し元（folder2iso.exe）は UTF-8 で読む。既定のままだとコンソールのコードページ（日本語なら CP932）で出る
    [Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false
}

# 人には色付きの文で、folder2iso.exe には "@@種類 内容" の 1 行で伝える。
# 種類: INFO（経過）/ WARN（警告）/ PROGRESS（書いた量 全体の量）/ DONE（できた大きさ）/ ERROR
function Say([string]$Message, [string]$Kind = 'INFO') {
    if ($Machine) {
        [Console]::Out.WriteLine("@@$Kind $Message")
        return
    }
    switch ($Kind) {
        'WARN' { Write-Host $Message -ForegroundColor Yellow }
        'DONE' { Write-Host $Message -ForegroundColor Green }
        'ERROR' { Write-Host ("エラー: " + $Message) -ForegroundColor Red }
        default { Write-Host $Message }
    }
}

function Fail([string]$Message) {
    Say $Message 'ERROR'
    exit 1
}

# IMAPI2 の定数（imapi2fs.h の FsiFileSystems と imapi2.h の IMAPI_MEDIA_PHYSICAL_TYPE）
$FsiIso9660 = 1
$FsiJoliet = 2
$FsiUdf = 4
$MediaTypes = @{
    'CD'        = 2   # IMAPI_MEDIA_TYPE_CDR
    'DVD'       = 6   # IMAPI_MEDIA_TYPE_DVDPLUSR
    'DVD-DL'    = 8   # IMAPI_MEDIA_TYPE_DVDPLUSR_DUALLAYER
    'BD'        = 18  # IMAPI_MEDIA_TYPE_BDR
    'BD-DL'     = 18  # 2 層は FreeMediaBlocks を下で倍にする
    'Unlimited' = 12  # IMAPI_MEDIA_TYPE_DISK
}
$FourGiB = 4GB

function Format-Size([long]$Bytes) {
    if ($Bytes -ge 1GB) { return '{0:N2} GB' -f ($Bytes / 1GB) }
    if ($Bytes -ge 1MB) { return '{0:N1} MB' -f ($Bytes / 1MB) }
    if ($Bytes -ge 1KB) { return '{0:N1} KB' -f ($Bytes / 1KB) }
    return "$Bytes B"
}

# IMAPI2 が返すイメージは COM の IStream で、PowerShell からは直接読めないので、C# で少しずつファイルへ写す。
# 隠しファイルを除くときは AddTree が使えず 1 つずつ AddFile するので、ファイルを IStream で開くのも C# で行う。
# C# 5 の書き方に留めるのは、Windows PowerShell 5.1 の Add-Type が古いコンパイラで動くため。
# 名前空間に版を入れるのは、同じ PowerShell の中で古い版の型が読み込み済みだと Add-Type が失敗するため。
if (-not ('Folder2IsoV2.StreamWriter' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Runtime.InteropServices.ComTypes;

namespace Folder2IsoV2 {
    public sealed class StreamWriter : IDisposable {
        private readonly IStream source;
        private readonly FileStream target;
        private readonly byte[] buffer = new byte[4 * 1024 * 1024];
        private readonly IntPtr readCount;

        public StreamWriter(object comStream, string path, bool overwrite) {
            source = (IStream)comStream;
            target = new FileStream(path, overwrite ? FileMode.Create : FileMode.CreateNew,
                                    FileAccess.Write, FileShare.None);
            readCount = Marshal.AllocHGlobal(sizeof(int));
        }

        // 最大 maxBytes を写し、写した量を返す。0 なら終わり。
        public long Step(long maxBytes) {
            long done = 0;
            while (done < maxBytes) {
                source.Read(buffer, buffer.Length, readCount);
                int n = Marshal.ReadInt32(readCount);
                if (n <= 0) break;
                target.Write(buffer, 0, n);
                done += n;
            }
            return done;
        }

        public void Dispose() {
            target.Flush(true);
            target.Dispose();
            Marshal.FreeHGlobal(readCount);
            Marshal.ReleaseComObject(source);
        }
    }

    public static class Files {
        [DllImport("shlwapi.dll", CharSet = CharSet.Unicode, PreserveSig = false)]
        private static extern void SHCreateStreamOnFileEx(string file, uint mode, uint attributes,
            bool create, IStream template, out IStream stream);

        // 読むだけの IStream で開く（STGM_READ | STGM_SHARE_DENY_WRITE）。中身はメモリに読み込まない。
        public static object OpenRead(string path) {
            IStream stream;
            SHCreateStreamOnFileEx(path, 0x00000020, 0, false, null, out stream);
            return stream;
        }
    }
}
'@
}

# 隠しファイルを除いた中身を、親フォルダが子より先に来る順で返す。
# Get-ChildItem を -Force なしで呼ぶと、隠し属性とシステム属性のものは返らない。
# リンク・ジャンクションをたどらないのは、ループしていると終わらないため。
function Get-VisibleItems([string]$Path) {
    foreach ($item in @(Get-ChildItem -LiteralPath $Path -ErrorAction SilentlyContinue)) {
        if ($item.Name.StartsWith('.')) { continue }
        if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { continue }
        $item
        if ($item.PSIsContainer) { Get-VisibleItems $item.FullName }
    }
}

# ---- 入力の確認 ----

$srcItem = Get-Item -LiteralPath $Source -ErrorAction SilentlyContinue
if ($null -eq $srcItem -or -not $srcItem.PSIsContainer) {
    Fail "フォルダが見つかりません: $Source"
}
$srcPath = $srcItem.FullName.TrimEnd('\')
$folderName = $srcItem.Name.TrimEnd('\', ':')
if ([string]::IsNullOrEmpty($folderName)) { $folderName = 'CDROM' }

if ([string]::IsNullOrWhiteSpace($Output)) {
    $parent = Split-Path -Parent $srcPath
    if ([string]::IsNullOrEmpty($parent)) { $parent = (Get-Location).Path }
    $Output = Join-Path $parent ($folderName + '.iso')
}
$outPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Output)
if ([string]::IsNullOrWhiteSpace($Label)) { $Label = $folderName }

if ((Test-Path -LiteralPath $outPath) -and -not $Force) {
    Fail "出力ファイルが既にあります（上書きするなら -Force）: $outPath"
}
# 出力先が元のフォルダの中だと、作った ISO を自分の中に取り込んでしまう
if ($outPath.StartsWith($srcPath + '\', [StringComparison]::OrdinalIgnoreCase)) {
    Fail "出力先を元のフォルダの中にはできません: $outPath"
}

Say "元のフォルダ : $srcPath"
Say "出力ファイル : $outPath"
Say "中身を数えています..."
if ($ExcludeHidden) {
    $visible = @(Get-VisibleItems $srcPath)
    $files = @($visible | Where-Object { -not $_.PSIsContainer })
}
else {
    $files = @(Get-ChildItem -LiteralPath $srcPath -Recurse -Force -File -ErrorAction SilentlyContinue)
}
$totalBytes = [long]0
$bigFiles = @()
foreach ($f in $files) {
    $totalBytes += $f.Length
    if ($f.Length -ge $FourGiB) { $bigFiles += $f }
}
Say ("中身         : {0} ファイル、{1}" -f $files.Count, (Format-Size $totalBytes))

$fs = $FsiIso9660 -bor $FsiJoliet -bor $FsiUdf
switch ($FileSystem) {
    'UDF' { $fs = $FsiUdf }
    'Auto' {
        if ($bigFiles.Count -gt 0) {
            $fs = $FsiUdf
            Say ("4GiB 以上のファイルが {0} 個あるので、UDF だけで作ります（Windows 10/11 では問題なく開けます）" -f $bigFiles.Count) 'WARN'
        }
    }
    'All' {
        if ($bigFiles.Count -gt 0) {
            Fail ("4GiB 以上のファイルは ISO9660/Joliet に入りません。-FileSystem Auto か UDF にしてください: " + (($bigFiles | ForEach-Object { $_.FullName }) -join ', '))
        }
    }
}
$fsNames = @()
if ($fs -band $FsiIso9660) { $fsNames += 'ISO9660' }
if ($fs -band $FsiJoliet) { $fsNames += 'Joliet' }
if ($fs -band $FsiUdf) { $fsNames += 'UDF' }
Say ("形式         : {0}、メディア: {1}、ラベル: {2}" -f ($fsNames -join ' + '), $Media, $Label)

# ---- イメージを作る ----

$image = $null
$writer = $null
$completed = $false
try {
    $image = New-Object -ComObject IMAPI2FS.MsftFileSystemImage
    $image.ChooseImageDefaultsForMediaType($MediaTypes[$Media])
    if ($Media -eq 'BD-DL') { $image.FreeMediaBlocks = $image.FreeMediaBlocks * 2 }
    # 既定の上限は CD 1 枚分などのメディアの容量。Unlimited は 0（上限なし）にする
    if ($Media -eq 'Unlimited') { $image.FreeMediaBlocks = 0 }
    $image.FileSystemsToCreate = $fs
    $image.VolumeName = $Label

    $capacity = [long]$image.FreeMediaBlocks * 2048
    if ($capacity -gt 0 -and $totalBytes -gt $capacity) {
        throw ("{0} に入りきりません（中身 {1}、容量 {2}）。メディアを大きいものか「上限なし」(Unlimited) にしてください" -f $Media, (Format-Size $totalBytes), (Format-Size $capacity))
    }

    Say "ファイルを追加しています（ファイルが多いと時間がかかります）..."
    if ($ExcludeHidden) {
        foreach ($item in $visible) {
            $rel = $item.FullName.Substring($srcPath.Length + 1)
            if ($item.PSIsContainer) {
                $image.Root.AddDirectory($rel)
            }
            else {
                $image.Root.AddFile($rel, [Folder2IsoV2.Files]::OpenRead($item.FullName))
            }
        }
    }
    else {
        # 第 2 引数 $false: フォルダそのものではなく、中身を ISO の直下に入れる
        $image.Root.AddTree($srcPath, $false)
    }

    Say "イメージを組み立てています..."
    $result = $image.CreateResultImage()
    $imageBytes = [long]$result.TotalBlocks * [long]$result.BlockSize

    Say "書き出しています..."
    $writer = New-Object Folder2IsoV2.StreamWriter -ArgumentList $result.ImageStream, $outPath, ([bool]$Force)
    $written = [long]0
    $started = Get-Date
    while ($true) {
        $n = $writer.Step(64MB)
        if ($n -le 0) { break }
        $written += $n
        if ($Machine) {
            Say "$written $imageBytes" 'PROGRESS'
            continue
        }
        # [Math]::Max(1, ...) だと int 版が選ばれ、2GB を超える大きさで変換に失敗するので long でそろえる
        $pct = [int][Math]::Min([double]100, $written * 100.0 / [Math]::Max([long]1, $imageBytes))
        $secs = ((Get-Date) - $started).TotalSeconds
        $status = '{0} / {1}' -f (Format-Size $written), (Format-Size $imageBytes)
        if ($secs -gt 1) { $status += ('  {0}/秒' -f (Format-Size ([long]($written / $secs)))) }
        Write-Progress -Activity 'ISO を書き出しています' -Status $status -PercentComplete $pct
    }
    if (-not $Machine) { Write-Progress -Activity 'ISO を書き出しています' -Completed }
    $writer.Dispose()
    $writer = $null
    $completed = $true
    if ($Machine) {
        Say "$written" 'DONE'
    }
    else {
        Say ("作成しました : {0}（{1}、{2:N1} 秒）" -f $outPath, (Format-Size $written), ((Get-Date) - $started).TotalSeconds) 'DONE'
    }
}
catch {
    Say $_.Exception.Message 'ERROR'
}
finally {
    $wroteSomething = $null -ne $writer
    if ($null -ne $writer) { $writer.Dispose() }
    if ($null -ne $image) { [void][Runtime.InteropServices.Marshal]::ReleaseComObject($image) }
    # 失敗・中断（Ctrl+C）で残った書きかけのファイルは消す
    if (-not $completed -and $wroteSomething -and (Test-Path -LiteralPath $outPath)) {
        Remove-Item -LiteralPath $outPath -Force
        Say "書きかけのファイルを消しました: $outPath"
    }
}
if (-not $completed) { exit 1 }
exit 0
