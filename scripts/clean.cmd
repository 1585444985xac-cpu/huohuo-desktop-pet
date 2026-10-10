@echo off
REM ---------------------------------------------------------------
REM  clean.cmd - launcher for clean.ps1
REM
REM  This machine's PowerShell execution policy is Restricted, so
REM  running a .ps1 directly is refused ("running scripts is disabled
REM  on this system"). The -ExecutionPolicy Bypass below lifts the
REM  restriction for this one process only; it does NOT change any
REM  machine-wide setting.
REM
REM  NOTE: keep this file ASCII-only. cmd.exe reads .bat/.cmd using the
REM  console code page (GBK on Chinese Windows), so non-ASCII bytes in
REM  comments get mis-parsed and are executed as commands.
REM
REM  Usage is identical to clean.ps1:
REM      scripts\clean.cmd -WhatIf
REM      scripts\clean.cmd -IncrementalOnly
REM      scripts\clean.cmd -Target
REM      scripts\clean.cmd -All
REM ---------------------------------------------------------------

powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0clean.ps1" %*
exit /b %ERRORLEVEL%
