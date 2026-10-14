# Regenerate NSIS artwork from the existing app logo, without a design-tool dependency.
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$logo = [Drawing.Image]::FromFile((Join-Path $PSScriptRoot '../../icons/icon.png'))
try {
    foreach ($kind in @('header', 'sidebar')) {
        $width = if ($kind -eq 'header') { 150 } else { 164 }
        $height = if ($kind -eq 'header') { 57 } else { 314 }
        # NSIS expects a Windows bitmap; 24-bit RGB avoids alpha/palette differences.
        $bitmap = [Drawing.Bitmap]::new($width, $height, [Drawing.Imaging.PixelFormat]::Format24bppRgb)
        $graphics = [Drawing.Graphics]::FromImage($bitmap)
        $font = $null
        $background = $null
        $format = [Drawing.StringFormat]::new()
        try {
            $graphics.InterpolationMode = [Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
            $graphics.TextRenderingHint = [Drawing.Text.TextRenderingHint]::AntiAliasGridFit
            $format.Alignment = [Drawing.StringAlignment]::Center
            $format.LineAlignment = [Drawing.StringAlignment]::Center
            if ($kind -eq 'header') {
                # Match MUI_BGCOLOR in appearance.nsh so the header has no visible seam.
                $graphics.Clear([Drawing.Color]::FromArgb(249, 250, 251))
                $graphics.DrawImage($logo, 112, 12, 33, 33)
                $font = [Drawing.Font]::new('Segoe UI', 12, [Drawing.FontStyle]::Regular, [Drawing.GraphicsUnit]::Pixel)
                $graphics.DrawString('squeezegram', $font, [Drawing.Brushes]::Black,
                    [Drawing.RectangleF]::new(0, 0, 108, 57), $format)
            }
            else {
                $background = [Drawing.Drawing2D.LinearGradientBrush]::new(
                    [Drawing.Point]::new(0, 0), [Drawing.Point]::new(0, $height),
                    [Drawing.Color]::FromArgb(38, 38, 38), [Drawing.Color]::FromArgb(16, 16, 16))
                $graphics.FillRectangle($background, 0, 0, $width, $height)
                $graphics.DrawImage($logo, 39, 67, 86, 86)
                $font = [Drawing.Font]::new('Segoe UI', 18, [Drawing.FontStyle]::Regular, [Drawing.GraphicsUnit]::Pixel)
                $graphics.DrawString('squeezegram', $font, [Drawing.Brushes]::White,
                    [Drawing.RectangleF]::new(0, 165, $width, 30), $format)
            }
            $bitmap.Save((Join-Path $PSScriptRoot "$kind.bmp"), [Drawing.Imaging.ImageFormat]::Bmp)
        }
        finally {
            if ($null -ne $background) { $background.Dispose() }
            if ($null -ne $font) { $font.Dispose() }
            $format.Dispose()
            $graphics.Dispose()
            $bitmap.Dispose()
        }
    }
}
finally { $logo.Dispose() }
