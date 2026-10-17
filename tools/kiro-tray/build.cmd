@echo off
cd /d "%~dp0"
"%WINDIR%\Microsoft.NET\Framework64\v4.0.30319\csc.exe" /nologo /target:winexe /optimize+ /out:KiroTray.exe ^
  /win32icon:icon_wait.ico ^
  /r:System.Windows.Forms.dll /r:System.Drawing.dll KiroTray.cs
if errorlevel 1 (echo 编译失败 & pause & exit /b 1)
echo 编译完成: %~dp0KiroTray.exe
