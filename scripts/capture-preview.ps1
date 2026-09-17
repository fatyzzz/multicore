param(
    [Parameter(Mandatory = $true)]
    [int]$ProcessId,
    [Parameter(Mandatory = $true)]
    [string]$OutputPath,
    [Parameter(Mandatory = $true)]
    [int]$Width,
    [Parameter(Mandatory = $true)]
    [int]$Height,
    [Parameter(Mandatory = $true)]
    [ValidateSet("empty", "ready", "connected", "glow-hover", "populated-catalog", "announcement", "error", "selection-pending", "diagnostics", "settings")]
    [string]$ExpectedState,
    [switch]$HoverConnection,
    [switch]$SelectSecondRoute,
    [switch]$OpenDiagnostics,
    [switch]$OpenSettings
)

$ErrorActionPreference = "Stop"

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;

public static class PreviewWindow {
    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left, Top, Right, Bottom; }

    [DllImport("user32.dll", SetLastError = true)]
    public static extern bool SetWindowPos(IntPtr hWnd, IntPtr hWndInsertAfter, int x, int y, int cx, int cy, uint flags);

    [DllImport("user32.dll")]
    public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);

    [DllImport("user32.dll")]
    public static extern bool SetForegroundWindow(IntPtr hWnd);

    [DllImport("user32.dll")]
    public static extern bool BringWindowToTop(IntPtr hWnd);

    [DllImport("user32.dll")]
    public static extern bool ShowWindowAsync(IntPtr hWnd, int command);

    [DllImport("user32.dll")]
    private static extern bool PostMessage(IntPtr window, uint message, IntPtr wParam, IntPtr lParam);

    [StructLayout(LayoutKind.Sequential)]
    public struct Point {
        public int X;
        public int Y;
    }

    [DllImport("user32.dll")]
    private static extern bool ClientToScreen(IntPtr window, ref Point point);

    [DllImport("user32.dll")]
    private static extern bool GetCursorPos(out Point point);

    [DllImport("user32.dll")]
    private static extern bool SetCursorPos(int x, int y);

    public static void ClickClient(IntPtr window, int x, int y) {
        IntPtr position = (IntPtr)((y << 16) | (x & 0xffff));
        PostMessage(window, 0x0200, IntPtr.Zero, position);
        PostMessage(window, 0x0201, (IntPtr)1, position);
        PostMessage(window, 0x0202, IntPtr.Zero, position);
    }

    public static void MoveClient(IntPtr window, int x, int y) {
        Point screen = new Point { X = x, Y = y };
        ClientToScreen(window, ref screen);
        SetCursorPos(screen.X, screen.Y);
        IntPtr position = (IntPtr)((y << 16) | (x & 0xffff));
        PostMessage(window, 0x0200, IntPtr.Zero, position);
    }

    public static Point GetCursorPosition() {
        Point point;
        GetCursorPos(out point);
        return point;
    }

    public static void RestoreCursor(Point point) {
        SetCursorPos(point.X, point.Y);
    }

    [DllImport("user32.dll", SetLastError = true)]
    public static extern bool PrintWindow(IntPtr hWnd, IntPtr hdcBlt, uint flags);

    [DllImport("dwmapi.dll")]
    public static extern int DwmFlush();
}
"@

function Get-PreviewWindow {
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    do {
        $process = Get-Process -Id $ProcessId -ErrorAction Stop
        $process.Refresh()
        if ($process.MainWindowHandle -ne [IntPtr]::Zero) {
            return $process.MainWindowHandle
        }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Desktop window did not become available."
}

function Set-PreviewSize([IntPtr]$Handle) {
    [void][PreviewWindow]::ShowWindowAsync($Handle, 9)
    if (-not [PreviewWindow]::SetWindowPos($Handle, [IntPtr]::Zero, 80, 60, $Width, $Height, 0x0040)) {
        throw "SetWindowPos failed."
    }
    [void][PreviewWindow]::BringWindowToTop($Handle)
    [void][PreviewWindow]::SetForegroundWindow($Handle)
    [void][PreviewWindow]::DwmFlush()
    Start-Sleep -Milliseconds 700
}

function Click-Preview([IntPtr]$Handle, [int]$ClientX, [int]$ClientY) {
    [PreviewWindow]::ClickClient($Handle, $ClientX, $ClientY)
    Start-Sleep -Milliseconds 350
}

function Move-Preview([IntPtr]$Handle, [int]$ClientX, [int]$ClientY) {
    [PreviewWindow]::MoveClient($Handle, $ClientX, $ClientY)
    Start-Sleep -Milliseconds 450
}

function Test-ColorNear(
    [System.Drawing.Color]$Actual,
    [int]$Red,
    [int]$Green,
    [int]$Blue,
    [int]$Tolerance = 6
) {
    return [Math]::Abs($Actual.R - $Red) -le $Tolerance -and
        [Math]::Abs($Actual.G - $Green) -le $Tolerance -and
        [Math]::Abs($Actual.B - $Blue) -le $Tolerance
}

function Get-BrightPixelCount(
    [System.Drawing.Bitmap]$Bitmap,
    [int]$Left,
    [int]$Top,
    [int]$Right,
    [int]$Bottom
) {
    $count = 0
    for ($y = $Top; $y -le $Bottom; $y++) {
        for ($x = $Left; $x -le $Right; $x++) {
            $color = $Bitmap.GetPixel($x, $y)
            if (($color.R + $color.G + $color.B) -gt 420) {
                $count++
            }
        }
    }
    return $count
}

function Get-SemanticPixelCount(
    [System.Drawing.Bitmap]$Bitmap,
    [int]$Red,
    [int]$Green,
    [int]$Blue
) {
    $count = 0
    for ($y = 31; $y -lt $Bitmap.Height; $y += 2) {
        for ($x = 8; $x -lt $Bitmap.Width; $x += 2) {
            if (Test-ColorNear $Bitmap.GetPixel($x, $y) $Red $Green $Blue 12) {
                $count++
            }
        }
    }
    return $count
}

function Get-SemanticPixelCountInRegion(
    [System.Drawing.Bitmap]$Bitmap,
    [int]$Red,
    [int]$Green,
    [int]$Blue,
    [int]$Left,
    [int]$Top,
    [int]$Right,
    [int]$Bottom
) {
    $count = 0
    for ($y = $Top; $y -le $Bottom; $y++) {
        for ($x = $Left; $x -le $Right; $x++) {
            if (Test-ColorNear $Bitmap.GetPixel($x, $y) $Red $Green $Blue 12) {
                $count++
            }
        }
    }
    return $count
}

function Test-PreviewFrame([System.Drawing.Bitmap]$Bitmap, [ref]$Failure) {
    $mid = [Math]::Floor($Bitmap.Width / 2)
    if ($ExpectedState -eq "diagnostics") {
        if (-not (Test-ColorNear $Bitmap.GetPixel($mid, 15) 10 12 15 8)) {
            $Failure.Value = "missing title-bar background"
            return $false
        }
        if ((Get-BrightPixelCount $Bitmap 20 60 ($Bitmap.Width - 20) ($Bitmap.Height - 20)) -lt 120) {
            $Failure.Value = "missing diagnostics text"
            return $false
        }
        if ((Get-SemanticPixelCount $Bitmap 60 203 127) -lt 3) {
            $Failure.Value = "missing ready health indicators"
            return $false
        }
        return $true
    }
    $checks = @(
        @{ Pass = (Test-ColorNear $Bitmap.GetPixel($mid, 15) 10 12 15 8); Name = "title-bar background" },
        @{ Pass = (Test-ColorNear $Bitmap.GetPixel(10, 40) 10 12 15 6); Name = "navigation rail" },
        @{ Pass = (Test-ColorNear $Bitmap.GetPixel($Bitmap.Width - 10, $Bitmap.Height - 10) 13 15 18 6); Name = "main canvas" },
        @{ Pass = ((Get-BrightPixelCount $Bitmap 16 36 ($Bitmap.Width - 16) ($Bitmap.Height - 16)) -ge 160); Name = "control-center content" }
    )
    foreach ($check in $checks) {
        if (-not $check.Pass) {
            $Failure.Value = "missing $($check.Name) anchor"
            return $false
        }
    }

    if ($ExpectedState -notin @("empty", "settings")) {
        $routeTop = [Math]::Min(400, $Bitmap.Height - 120)
        if ((Get-BrightPixelCount $Bitmap 20 $routeTop ($Bitmap.Width - 20) ($Bitmap.Height - 12)) -lt 35) {
            $Failure.Value = "missing populated route-region text"
            return $false
        }
    }
    if ($ExpectedState -eq "connected" -and (Get-SemanticPixelCount $Bitmap 60 203 127) -lt 3) {
        $Failure.Value = "missing connected semantic color"
        return $false
    }
    if ($ExpectedState -eq "error" -and (Get-SemanticPixelCount $Bitmap 240 106 106) -lt 3) {
        $Failure.Value = "missing error semantic color"
        return $false
    }
    if ($ExpectedState -eq "selection-pending") {
        $pendingHeaderPixels = Get-SemanticPixelCountInRegion $Bitmap 231 170 69 ($Bitmap.Width - 180) 238 ($Bitmap.Width - 16) 270
        $pendingGutterPixels = Get-SemanticPixelCountInRegion $Bitmap 231 170 69 ($Bitmap.Width - 55) 396 ($Bitmap.Width - 15) 445
        if ($pendingHeaderPixels -lt 3 -or $pendingGutterPixels -lt 2) {
            $Failure.Value = "missing pending header or selected-row indicator"
            return $false
        }
    }
    return $true
}

function Get-PreviewFrameSignature([System.Drawing.Bitmap]$Bitmap) {
    $stream = [System.IO.MemoryStream]::new()
    try {
        $Bitmap.Save($stream, [System.Drawing.Imaging.ImageFormat]::Png)
        $sha256 = [System.Security.Cryptography.SHA256]::Create()
        try {
            return [BitConverter]::ToString($sha256.ComputeHash($stream.ToArray())).Replace("-", "")
        } finally {
            $sha256.Dispose()
        }
    } finally {
        $stream.Dispose()
    }
}

function Save-Preview([IntPtr]$Handle) {
    $rect = [PreviewWindow+RECT]::new()
    if (-not [PreviewWindow]::GetWindowRect($Handle, [ref]$rect)) {
        throw "GetWindowRect failed."
    }
    $captureWidth = $rect.Right - $rect.Left
    $captureHeight = $rect.Bottom - $rect.Top
    if ($captureWidth -ne $Width -or $captureHeight -ne $Height) {
        throw "Window size mismatch: requested ${Width}x${Height}, got ${captureWidth}x${captureHeight}."
    }
    $directory = Split-Path -Parent $OutputPath
    New-Item -ItemType Directory -Force -Path $directory | Out-Null
    $requiredStableFrames = 2
    $stableFrames = 0
    $previousSignature = ""
    $lastFailure = "no frame rendered"
    for ($attempt = 1; $attempt -le 20; $attempt++) {
        [void][PreviewWindow]::BringWindowToTop($Handle)
        [void][PreviewWindow]::SetForegroundWindow($Handle)
        [void][PreviewWindow]::DwmFlush()
        $bitmap = [System.Drawing.Bitmap]::new(
            $captureWidth,
            $captureHeight,
            [System.Drawing.Imaging.PixelFormat]::Format24bppRgb
        )
        try {
            $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
            try {
                $deviceContext = $graphics.GetHdc()
                try {
                    # PW_RENDERFULLCONTENT asks DWM for the entire native window.
                    if (-not [PreviewWindow]::PrintWindow($Handle, $deviceContext, 2)) {
                        $lastFailure = "PrintWindow returned false"
                        continue
                    }
                } finally {
                    $graphics.ReleaseHdc($deviceContext)
                }
            } finally {
                $graphics.Dispose()
            }

            $frameFailure = ""
            if (-not (Test-PreviewFrame $bitmap ([ref]$frameFailure))) {
                $lastFailure = $frameFailure
                $stableFrames = 0
                $previousSignature = ""
                continue
            }

            $signature = Get-PreviewFrameSignature $bitmap
            if ($signature -eq $previousSignature) {
                $stableFrames++
            } else {
                $previousSignature = $signature
                $stableFrames = 1
            }
            if ($stableFrames -ge $requiredStableFrames) {
                $bitmap.Save($OutputPath, [System.Drawing.Imaging.ImageFormat]::Png)
                Write-Output "$OutputPath`t${captureWidth}x${captureHeight}`tvalidated-attempt=$attempt"
                return
            }
        } finally {
            $bitmap.Dispose()
        }
        Start-Sleep -Milliseconds 250
    }
    throw "Preview frame validation failed after 20 attempts for $ExpectedState`: $lastFailure"
}

$originalCursor = [PreviewWindow]::GetCursorPosition()
try {
    $handle = Get-PreviewWindow
    # A non-zero HWND can be published before Slint's first frame and async refresh.
    Start-Sleep -Seconds 2
    Set-PreviewSize $handle
    if ($HoverConnection) {
        # Cross the power-icon boundary before settling on the left side of the
        # same pill. The mesh must keep one continuous coordinate source.
        Move-Preview $handle 300 112
        Move-Preview $handle 395 112
        Move-Preview $handle 300 112
    }
    if ($SelectSecondRoute) {
        Click-Preview $handle ([Math]::Floor(($Width + 176) / 2)) 420
    }
    if ($OpenDiagnostics) {
        Click-Preview $handle 88 122
    }
    if ($OpenSettings) {
        Click-Preview $handle 88 174
    }
    Save-Preview $handle
} finally {
    [PreviewWindow]::RestoreCursor($originalCursor)
}
