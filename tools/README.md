# folder2DVD_ISO.ps1

フォルダを指定すると、その中身をまるごと入れた **ISO イメージファイル（.iso）** を作る PowerShell スクリプトです。
ISO を作る処理は、Windows に標準で入っている **IMAPI2**（Image Mastering API v2、`imapi2fs.dll`）に任せています。
そのため、ソフトを追加で入れなくても使えます。

**Windows の `folder2iso.exe` は、このスクリプトを呼んで ISO を作ります。** exe と同じフォルダに置いてください。
画面がいらないときは、このスクリプトだけを PowerShell から直接使うこともできます。

---

## できること

- 指定したフォルダの中身を、サブフォルダも含めて 1 つの `.iso` にまとめます。
- 形式は **ISO9660 + Joliet + UDF** です。日本語のファイル名もそのまま入ります。
- **4GiB 以上のファイルがあるときは、自動で UDF だけで作ります。** Windows 10/11 は UDF の ISO もダブルクリックで開けます。
- 焼く予定のメディア（CD / DVD / 2 層 DVD / BD / 2 層 BD）を指定できます。中身が入りきらないときは、作り始める前に止まります。
  `-Media Unlimited` にすると容量の上限はなくなります（焼かずにファイルとして使う場合はこれ）。
- ボリュームラベル（ドライブ名の横に出る名前）を付けられます。省略するとフォルダ名になります。
- 書き出しの進み具合（％、量、速さ）を PowerShell の進行状況バーに表示します。
- 既にあるファイルは、`-Force` を付けない限り上書きしません。
- 失敗したときや、Ctrl+C で止めたときは、書きかけの .iso を消します。

## できないこと・注意

- **ISO の中身は圧縮されません。** ISO の大きさは、入れたファイルの合計とほぼ同じになります。
- 起動できる（ブータブル）ISO は作れません。
- IMAPI2 は、ファイルを追加する段階（「ファイルを追加しています」）では進み具合を出しません。ファイルが数万個あると、ここでしばらく止まったように見えます。
- `-ExcludeHidden` を付けないときは、フォルダを IMAPI2 の `AddTree` でまとめて入れます。このとき IMAPI2 が隠しファイルも入れるかは確認できていません。付けたときは、隠していないものだけを 1 つずつ `AddFile` で入れます。
- ラベルに日本語や記号を使うと、IMAPI2 がエラーにすることがあります。そのときは英数字のラベルを `-Label` で指定してください。

## 前提条件

| 項目 | 内容 |
| --- | --- |
| OS | Windows 10 / 11（IMAPI2 は Vista 以降に標準で入っています） |
| PowerShell | Windows PowerShell 5.1（Windows に標準）、または PowerShell 7 以降 |
| 管理者権限 | 不要 |
| 空き容量 | 出力先のドライブに、元のフォルダと同じくらいの空き |
| 文字コード | スクリプトは **UTF-8（BOM 付き）** で保存してあります。編集したときも BOM 付きで保存してください。BOM がないと、Windows PowerShell 5.1 が日本語を読み違えて動かなくなります |

### 初めて実行するとき（実行ポリシー）

Windows の初期設定では、ネットから落としたスクリプトは実行できません。次のどれかで実行できるようになります。

```powershell
# A. このファイルだけ許可する（おすすめ）
Unblock-File .\folder2DVD_ISO.ps1

# B. 自分のユーザーで、署名のないローカルのスクリプトを許可する（1 回だけ設定すればよい）
Set-ExecutionPolicy -Scope CurrentUser RemoteSigned

# C. 設定を変えず、その 1 回だけ実行する
powershell -ExecutionPolicy Bypass -File .\folder2DVD_ISO.ps1 "D:\写真\2024"
```

## 使い方

PowerShell を開き、スクリプトのあるフォルダに移動して実行します。

```text
.\folder2DVD_ISO.ps1 [-Source] <元のフォルダ> [[-Output] <出力する.iso>] [-Label <ラベル>]
                     [-Media CD|DVD|DVD-DL|BD|BD-DL|Unlimited] [-FileSystem Auto|All|UDF] [-Force]
```

| パラメータ | 意味 | 既定 |
| --- | --- | --- |
| `-Source`（1 つ目） | ISO に入れるフォルダ（必須） | |
| `-Output`（2 つ目） | 出力する .iso ファイル | 元のフォルダと同じ場所の `フォルダ名.iso` |
| `-Label` | ボリュームラベル | フォルダ名 |
| `-Media` | 焼く予定のメディア。容量を超えると作る前に止める。CD = 700MB、DVD = 4.7GB、DVD-DL = 8.5GB、BD = 25GB、BD-DL = 50GB、Unlimited = 上限なし | `DVD` |
| `-FileSystem` | `Auto` = ISO9660 + Joliet + UDF（4GiB 以上のファイルがあれば UDF だけ）／`All` = 常に 3 つ全部／`UDF` = UDF だけ | `Auto` |
| `-ExcludeHidden` | 隠しファイル・隠しフォルダ（隠し属性かシステム属性のもの、名前が `.` で始まるもの）を入れない。リンク・ジャンクションも飛ばす | 入れる |
| `-Force` | 出力ファイルが既にあれば上書きする | 上書きしない |
| `-Machine` | folder2iso.exe から呼ぶとき用。1 行ずつ `@@種類 内容`（INFO / WARN / PROGRESS / DONE / ERROR）を UTF-8 で出す。手で使うときは要らない | |

詳しいヘルプは `Get-Help .\folder2DVD_ISO.ps1 -Detailed` で見られます。

### 例

```powershell
# D:\写真\2024 を D:\写真\2024.iso にする（DVD 1 枚に入るか確かめる）
.\folder2DVD_ISO.ps1 "D:\写真\2024"

# 大きなフォルダを、容量の上限なしで別のドライブに作る
.\folder2DVD_ISO.ps1 "D:\動画" "E:\iso\movies.iso" -Media Unlimited

# BD 用。ラベルを付けて、既にあれば上書き
.\folder2DVD_ISO.ps1 -Source "D:\archive\2023" -Output "E:\2023.iso" -Label "ARCHIVE2023" -Media BD -Force
```

実行すると次のように表示されます。

```text
元のフォルダ : D:\写真\2024
出力ファイル : D:\写真\2024.iso
中身を数えています...
中身         : 812 ファイル、3.21 GB
形式         : ISO9660 + Joliet + UDF、メディア: DVD、ラベル: 2024
ファイルを追加しています（ファイルが多いと時間がかかります）...
イメージを組み立てています...
作成しました : D:\写真\2024.iso（3.22 GB、41.3 秒）
```

- 成功すると終了コード `0`、失敗すると `1` を返します（`$LASTEXITCODE` か、バッチなら `errorlevel` で分かります）。

### バッチファイルから呼ぶ

```bat
@echo off
rem D:\archive の直下のフォルダを、それぞれ E:\iso\<フォルダ名>.iso にする
for /d %%D in ("D:\archive\*") do (
    powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0folder2DVD_ISO.ps1" "%%D" "E:\iso\%%~nxD.iso" -Media Unlimited
    if errorlevel 1 echo 失敗: %%D
)
```

## 作った ISO を使う

- **開く**: エクスプローラで `.iso` をダブルクリック（または右クリック → **マウント**）。
- **焼く**: 右クリック → **ディスク イメージの書き込み**。

## 困ったとき

| 症状 | 対処 |
| --- | --- |
| 「このシステムではスクリプトの実行が無効になっている」と出る | [実行ポリシー](#初めて実行するとき実行ポリシー) を参照 |
| 日本語が化ける、または構文エラーが出る | ファイルを BOM 付きの UTF-8 で保存し直してください |
| 「DVD に入りきりません」と出る | `-Media DVD-DL` / `BD` / `Unlimited` を指定してください |
| `0xC0AAB...` のような IMAPI のエラーが出る | ラベルを英数字にする、`-FileSystem UDF` にする、パスの長すぎるファイルを探す、を試してください |
| 「ファイルを追加しています」から進まない | ファイル数が多いと時間がかかります。タスクマネージャーで PowerShell の CPU 使用率が動いていれば処理中です |

## 仕組み（ざっくり）

1. `IMAPI2FS.MsftFileSystemImage`（COM）を作る
2. `ChooseImageDefaultsForMediaType` でメディアに合わせた既定を入れ、`FileSystemsToCreate` で形式、`VolumeName` でラベルを決める（`Unlimited` のときは `FreeMediaBlocks = 0` で上限をなくす）
3. `Root.AddTree(フォルダ, $false)` でフォルダの中身を追加する（`-ExcludeHidden` のときは、隠していないものを `AddDirectory` / `AddFile` で 1 つずつ）
4. `CreateResultImage()` で結果を受け取り、`ImageStream`（COM の `IStream`）を、スクリプトの中の小さな C# のクラスで少しずつファイルに写す
