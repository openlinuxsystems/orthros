# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Windows' chkdsk on each NTFS volume Radix's host tests wrote, and the
# volume's files read back as Windows reads them (ADR 0238). CI's Windows
# job runs it, on the volumes the unit job keeps; see
# kernel/fs/ntfs/src/windows_tests.rs for what they are. It needs
# PowerShell 7 and an administrator, to mount the disks.
#
#   pwsh tools/windows/chkdsk.ps1 <directory of the .vhd and .txt files>
#
# A volume a power cut left (its name ends in -cut) is mended with
# `chkdsk /f` first. Then `chkdsk` must find nothing wrong, each path its
# .txt lists must be there, a directory, a link, a file or a file's
# stream of the bytes it gives, and, but for a cut volume and one whose
# list names only what a test changed (-some), nothing else may be there
# but what Windows makes itself.

param([Parameter(Mandatory)] [string] $Images)

$ErrorActionPreference = 'Stop'
$vhds = @(Get-ChildItem -LiteralPath $Images -Filter *.vhd | Sort-Object Name)
if ($vhds.Count -eq 0) {
    throw "No volumes in $Images"
}
# What Windows makes on a volume it mounts.
$own = @('System Volume Information', '$RECYCLE.BIN')
$failed = 0
foreach ($vhd in $vhds) {
    $name = $vhd.BaseName
    $cut = $name.EndsWith('-cut')
    $some = $name.EndsWith('-some')
    $problems = [System.Collections.Generic.List[string]]::new()
    $lines = @()
    $image = Mount-DiskImage -ImagePath $vhd.FullName -PassThru
    try {
        $disk = $image | Get-Disk
        if ($disk.IsOffline) {
            Set-Disk -Number $disk.Number -IsOffline $false
        }
        if ($disk.IsReadOnly) {
            Set-Disk -Number $disk.Number -IsReadOnly $false
        }
        $partition = Get-Partition -DiskNumber $disk.Number -PartitionNumber 1
        if (-not $partition.DriveLetter) {
            $partition | Add-PartitionAccessPath -AssignDriveLetter
            $partition = Get-Partition -DiskNumber $disk.Number -PartitionNumber 1
        }
        $drive = "$($partition.DriveLetter):"
        if ($cut) {
            $out = & chkdsk.exe $drive /f /x 2>&1 | Out-String
            # 0: nothing wrong; 1: errors found and fixed.
            if ($LASTEXITCODE -gt 1) {
                $problems.Add("chkdsk /f exited with $LASTEXITCODE`n$out")
            }
        }
        $out = & chkdsk.exe $drive 2>&1 | Out-String
        if ($LASTEXITCODE -ne 0) {
            $problems.Add("chkdsk exited with $LASTEXITCODE`n$out")
        }
        $root = "$drive\"
        $listed = [System.Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
        $lines = Get-Content -LiteralPath (Join-Path $Images "$name.txt") -Encoding utf8
        foreach ($line in $lines) {
            if (-not $line) {
                continue
            }
            $what, $path = $line -split "`t", 2
            [void]$listed.Add($path)
            # A stream of a file or directory: `s <hash>`, its path
            # `path:stream` (ADR 0241).
            $stream = $null
            if ($what.StartsWith('s ')) {
                $at = $path.LastIndexOf(':')
                $stream = $path.Substring($at + 1)
                $path = $path.Substring(0, $at)
            }
            $full = Join-Path $root ($path -replace '/', '\')
            $item = Get-Item -LiteralPath $full -Force -ErrorAction SilentlyContinue
            if (-not $item) {
                $problems.Add("Missing: $path")
                continue
            }
            $link = [bool]($item.Attributes -band [IO.FileAttributes]::ReparsePoint)
            if ($stream) {
                # Opened as `file:stream`, as Windows names it; a
                # directory's too.
                try {
                    $bytes = [IO.File]::ReadAllBytes("$full`:$stream")
                } catch {
                    $problems.Add("Missing stream: $path`:$stream ($_)")
                    continue
                }
                $hash = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($bytes))
                if ("s $($hash.ToLowerInvariant())" -ne $what) {
                    $problems.Add("Other bytes: $path`:$stream")
                }
            } elseif ($what -eq 'd') {
                if (-not $item.PSIsContainer -or $link) {
                    $problems.Add("Not a directory: $path")
                }
            } elseif ($what -eq 'l') {
                if (-not $link) {
                    $problems.Add("Not a link: $path")
                }
            } else {
                $hash = (Get-FileHash -LiteralPath $full -Algorithm SHA256).Hash.ToLowerInvariant()
                if ($item.PSIsContainer -or $link -or "f $hash" -ne $what) {
                    $problems.Add("Other bytes: $path")
                }
            }
        }
        if (-not $cut -and -not $some) {
            # Links are listed, not followed.
            $found = Get-ChildItem -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
            foreach ($item in $found) {
                $path = $item.FullName.Substring($root.Length) -replace '\\', '/'
                $top = $path.Split('/')[0]
                if ($own -notcontains $top -and -not $listed.Contains($path)) {
                    $problems.Add("Not written: $path")
                }
            }
        }
    } catch {
        $problems.Add("$_")
    } finally {
        Dismount-DiskImage -ImagePath $vhd.FullName | Out-Null
    }
    if ($problems.Count -eq 0) {
        Write-Output "ok   $name ($($lines.Count) paths)"
    } else {
        $failed++
        Write-Output "FAIL $name"
        foreach ($problem in $problems) {
            Write-Output "     $problem"
        }
    }
}
Write-Output "$($vhds.Count - $failed) of $($vhds.Count) volumes passed"
if ($failed -ne 0) {
    exit 1
}
