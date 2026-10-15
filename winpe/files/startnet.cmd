@echo off
title inocrium reflect - Windows PE
wpeinit
rem High performance power plan: restores run noticeably faster.
powercfg /s 8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c >nul 2>&1
set PATH=%PATH%;X:\inocrium
powershell.exe -NoProfile -ExecutionPolicy Bypass -File X:\inocrium\Restore-Menu.ps1
echo.
echo Type "powershell -File X:\inocrium\Restore-Menu.ps1" to open the menu again, or "wpeutil reboot" to restart.
