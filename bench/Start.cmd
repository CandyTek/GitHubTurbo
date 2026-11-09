@echo off
cd /d "%~dp0"
python bench.py serve --open
if errorlevel 1 pause
