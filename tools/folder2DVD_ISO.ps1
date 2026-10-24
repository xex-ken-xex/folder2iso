<#
.SYNOPSIS
    フォルダから ISO イメージを作る（Windows 標準の IMAPI2 を使う）。

.DESCRIPTION
    指定したフォルダの中身をまるごと入れた .iso ファイルを作る。
    Windows に標準で入っている IMAPI2（imapi2fs.dll）にイメージを作らせるので、追加のソフトは要らない。
    UDF に対応しているので、4GiB 以上のファイルも入れられる（そのときは自動で UDF だけで作る）。

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

.PARAMETER Force
    出力ファイルが既にあれば上書きする。

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

    [switch]$Force
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'

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
# C# 5 の書き方に留めるのは、Windows PowerShell 5.1 の Add-Type が古いコンパイラで動くため。
if (-not ('Folder2Iso.StreamWriter' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Runtime.InteropServices.ComTypes;

namespace Folder2Iso {
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
}
'@
}

# ---- 入力の確認 ----

$srcItem = Get-Item -LiteralPath $Source -ErrorAction SilentlyContinue
if ($null -eq $srcItem -or -not $srcItem.PSIsContainer) {
    Write-Error "フォルダが見つかりません: $Source"
    exit 1
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
    Write-Error "出力ファイルが既にあります（上書きするなら -Force）: $outPath"
    exit 1
}
# 出力先が元のフォルダの中だと、作った ISO を自分の中に取り込んでしまう
if ($outPath.StartsWith($srcPath + '\', [StringComparison]::OrdinalIgnoreCase)) {
    Write-Error "出力先を元のフォルダの中にはできません: $outPath"
    exit 1
}

Write-Host "元のフォルダ : $srcPath"
Write-Host "出力ファイル : $outPath"
Write-Host "中身を数えています..."
$files = @(Get-ChildItem -LiteralPath $srcPath -Recurse -Force -File -ErrorAction SilentlyContinue)
$totalBytes = [long]0
$bigFiles = @()
foreach ($f in $files) {
    $totalBytes += $f.Length
    if ($f.Length -ge $FourGiB) { $bigFiles += $f }
}
Write-Host ("中身         : {0} ファイル、{1}" -f $files.Count, (Format-Size $totalBytes))

$fs = $FsiIso9660 -bor $FsiJoliet -bor $FsiUdf
switch ($FileSystem) {
    'UDF' { $fs = $FsiUdf }
    'Auto' {
        if ($bigFiles.Count -gt 0) {
            $fs = $FsiUdf
            Write-Host ("4GiB 以上のファイルが {0} 個あるので、UDF だけで作ります（Windows 10/11 では問題なく開けます）" -f $bigFiles.Count) -ForegroundColor Yellow
        }
    }
    'All' {
        if ($bigFiles.Count -gt 0) {
            Write-Error ("4GiB 以上のファイルは ISO9660/Joliet に入りません。-FileSystem Auto か UDF にしてください:`n  " + (($bigFiles | ForEach-Object { $_.FullName }) -join "`n  "))
            exit 1
        }
    }
}
$fsNames = @()
if ($fs -band $FsiIso9660) { $fsNames += 'ISO9660' }
if ($fs -band $FsiJoliet) { $fsNames += 'Joliet' }
if ($fs -band $FsiUdf) { $fsNames += 'UDF' }
Write-Host ("形式         : {0}、メディア: {1}、ラベル: {2}" -f ($fsNames -join ' + '), $Media, $Label)

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
        throw ("{0} に入りきりません（中身 {1}、容量 {2}）。-Media を大きいものか Unlimited にしてください" -f $Media, (Format-Size $totalBytes), (Format-Size $capacity))
    }

    Write-Host "ファイルを追加しています（ファイルが多いと時間がかかります）..."
    # 第 2 引数 $false: フォルダそのものではなく、中身を ISO の直下に入れる
    $image.Root.AddTree($srcPath, $false)

    Write-Host "イメージを組み立てています..."
    $result = $image.CreateResultImage()
    $imageBytes = [long]$result.TotalBlocks * [long]$result.BlockSize

    $writer = New-Object Folder2Iso.StreamWriter -ArgumentList $result.ImageStream, $outPath, ([bool]$Force)
    $written = [long]0
    $started = Get-Date
    while ($true) {
        $n = $writer.Step(64MB)
        if ($n -le 0) { break }
        $written += $n
        # [Math]::Max(1, ...) だと int 版が選ばれ、2GB を超える大きさで変換に失敗するので long でそろえる
        $pct = [int][Math]::Min([double]100, $written * 100.0 / [Math]::Max([long]1, $imageBytes))
        $secs = ((Get-Date) - $started).TotalSeconds
        $status = '{0} / {1}' -f (Format-Size $written), (Format-Size $imageBytes)
        if ($secs -gt 1) { $status += ('  {0}/秒' -f (Format-Size ([long]($written / $secs)))) }
        Write-Progress -Activity 'ISO を書き出しています' -Status $status -PercentComplete $pct
    }
    Write-Progress -Activity 'ISO を書き出しています' -Completed
    $completed = $true
    Write-Host ("作成しました : {0}（{1}、{2:N1} 秒）" -f $outPath, (Format-Size $written), ((Get-Date) - $started).TotalSeconds) -ForegroundColor Green
}
catch {
    Write-Host ("エラー: " + $_.Exception.Message) -ForegroundColor Red
}
finally {
    if ($null -ne $writer) { $writer.Dispose() }
    if ($null -ne $image) { [void][Runtime.InteropServices.Marshal]::ReleaseComObject($image) }
    # 失敗・中断（Ctrl+C）で残った書きかけのファイルは消す
    if (-not $completed -and $null -ne $writer -and (Test-Path -LiteralPath $outPath)) {
        Remove-Item -LiteralPath $outPath -Force
        Write-Host "書きかけのファイルを消しました: $outPath"
    }
}
if (-not $completed) { exit 1 }
exit 0
