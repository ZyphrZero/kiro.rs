Set WshShell = CreateObject("WScript.Shell")
WshShell.Run """c:\Users\User\kiro\target\release\kiro-rs.exe"" -c ""c:\Users\User\kiro\data\config.json"" --credentials ""c:\Users\User\kiro\data\credentials.json""", 0, False
