$ErrorActionPreference = 'Stop'

$source = Join-Path $PSScriptRoot 'dist\quarkdrive.exe'
$installDir = Join-Path $env:LOCALAPPDATA 'Programs\QuarkDrive'
$target = Join-Path $installDir 'quarkdrive.exe'

if (-not (Test-Path -LiteralPath $source)) {
    throw "未找到发布程序：$source"
}

New-Item -ItemType Directory -Path $installDir -Force | Out-Null
Copy-Item -LiteralPath $source -Destination $target -Force

$desktop = [Environment]::GetFolderPath('Desktop')
$shortcutPath = Join-Path $desktop '夸克网盘.lnk'
$shell = New-Object -ComObject WScript.Shell
$shortcut = $shell.CreateShortcut($shortcutPath)
$shortcut.TargetPath = $target
$shortcut.WorkingDirectory = $installDir
$shortcut.Description = '夸克网盘 Windows 挂载'
$shortcut.IconLocation = "$target,0"
$shortcut.Save()

$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
New-Item -Path $runKey -Force | Out-Null
Set-ItemProperty -Path $runKey -Name 'QuarkDrive' -Value ('"' + $target + '"')

Write-Host "安装完成：$target"
Write-Host "桌面快捷方式：$shortcutPath"
Write-Host '首次运行将打开设置页；请点击“二维码登录”，使用夸克网盘 APP 扫码。'

Start-Process -FilePath $target
