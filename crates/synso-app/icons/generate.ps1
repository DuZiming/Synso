# Regenerates the Synso placeholder icon set from one drawn master.
#
# The glyph is a neutral folder + magnifier (Task 19 controller ruling:
# simple placeholder, no third-party logos). Drawn once at 1024 px with
# GDI+ (System.Drawing, ships with Windows PowerShell 5.1), then
# high-quality downscaled to every size the bundle config names.
#
# Usage:  powershell -File generate.ps1
# Output: PNGs + icon.ico + tray.png next to this script; icon.icns is
#         NOT regenerated (macOS-only, not produced by System.Drawing).

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

$here = Split-Path -Parent $MyInvocation.MyCommand.Path

function Color([string]$hex) {
    return [System.Drawing.ColorTranslator]::FromHtml($hex)
}

function RoundRect([single]$x, [single]$y, [single]$w, [single]$h, [single]$r) {
    $p = New-Object System.Drawing.Drawing2D.GraphicsPath
    $d = 2 * $r
    $p.AddArc($x, $y, $d, $d, 180, 90)
    $p.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $p.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
    $p.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $p.CloseFigure()
    return $p
}

# ---- master drawing (1024x1024 design coordinates) -------------------------

$master = New-Object System.Drawing.Bitmap(1024, 1024)
$g = [System.Drawing.Graphics]::FromImage($master)
$g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias

# Background: deep slate rounded square.
$bgBrush = New-Object System.Drawing.SolidBrush (Color "#2f3a48")
$g.FillPath($bgBrush, (RoundRect 32 32 960 960 200))

# Folder: amber, a back tab plus a body.
$folderBrush = New-Object System.Drawing.SolidBrush (Color "#f0a63c")
$g.FillPath($folderBrush, (RoundRect 170 280 330 130 48))   # tab
$g.FillPath($folderBrush, (RoundRect 170 350 660 410 48))   # body

# Magnifier: white ring + handle, overlapping the folder's right half.
$ringPen = New-Object System.Drawing.Pen ((Color "#ffffff"), 62)
$g.DrawEllipse($ringPen, 425, 385, 350, 350)                 # ring (center 600,560; r 175)
$handlePen = New-Object System.Drawing.Pen ((Color "#ffffff"), 92)
$handlePen.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
$handlePen.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
$g.DrawLine($handlePen, 722, 692, 856, 826)                  # handle

$g.Dispose()

# ---- downscale to every bundled size ----------------------------------------

function Save-Png([int]$size, [string]$name) {
    $out = New-Object System.Drawing.Bitmap($size, $size)
    $og = [System.Drawing.Graphics]::FromImage($out)
    $og.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $og.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $og.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $og.DrawImage($master, 0, 0, $size, $size)
    $og.Dispose()
    $out.Save((Join-Path $here $name), [System.Drawing.Imaging.ImageFormat]::Png)
    $out.Dispose()
}

# Files named by tauri.conf.json's bundle.icon + the UWP asset names.
Save-Png 32  "32x32.png"
Save-Png 128 "128x128.png"
Save-Png 256 "128x128@2x.png"
Save-Png 512 "icon.png"
Save-Png 30  "Square30x30Logo.png"
Save-Png 44  "Square44x44Logo.png"
Save-Png 71  "Square71x71Logo.png"
Save-Png 89  "Square89x89Logo.png"
Save-Png 107 "Square107x107Logo.png"
Save-Png 142 "Square142x142Logo.png"
Save-Png 150 "Square150x150Logo.png"
Save-Png 284 "Square284x284Logo.png"
Save-Png 310 "Square310x310Logo.png"
Save-Png 50  "StoreLogo.png"

# The tray icon: a dedicated 32 px PNG (the size Windows draws tray icons
# at), embedded into the binary via tauri::include_image!.
Save-Png 32 "tray.png"

# icon.ico: PNG-compressed entries (Vista+ shell format), sizes 16..256.
# Payloads first (deterministic encoder), then one header block + payloads.
$sizes = @(16, 24, 32, 48, 64, 128, 256)
$payloads = @()
foreach ($s in $sizes) {
    $png = New-Object System.IO.MemoryStream
    $tmp = New-Object System.Drawing.Bitmap($s, $s)
    $tg = [System.Drawing.Graphics]::FromImage($tmp)
    $tg.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $tg.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $tg.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $tg.DrawImage($master, 0, 0, $s, $s)
    $tg.Dispose()
    $tmp.Save($png, [System.Drawing.Imaging.ImageFormat]::Png)
    $tmp.Dispose()
    $payloads += , $png.ToArray()
    $png.Dispose()
}

$ms = New-Object System.IO.MemoryStream
$bw = New-Object System.IO.BinaryWriter($ms)
$bw.Write([uint16]0)                 # reserved
$bw.Write([uint16]1)                 # type: icon
$bw.Write([uint16]$sizes.Count)      # entry count
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $s = $sizes[$i]
    $bw.Write([byte]($(if ($s -ge 256) { 0 } else { $s })))  # width (0 = 256)
    $bw.Write([byte]($(if ($s -ge 256) { 0 } else { $s })))  # height
    $bw.Write([byte]0)               # palette
    $bw.Write([byte]0)               # reserved
    $bw.Write([uint16]1)             # color planes
    $bw.Write([uint16]32)            # bits per pixel
    $bw.Write([uint32]$payloads[$i].Length)  # data size
    $bw.Write([uint32]$offset)       # data offset
    $offset += $payloads[$i].Length
}
foreach ($p in $payloads) { $bw.Write($p) }
$bw.Flush()
[System.IO.File]::WriteAllBytes((Join-Path $here "icon.ico"), $ms.ToArray())
$bw.Dispose()
$ms.Dispose()

$master.Dispose()
Write-Host "icons regenerated in $here"
