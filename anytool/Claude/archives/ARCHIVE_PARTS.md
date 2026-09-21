# Large archive

`anthropic-ai-claude-code-2.1.88.tar` is stored as two parts because this Gitee repository does not support Git LFS.

Run this PowerShell code in this directory to rebuild the original archive:

```powershell
$parts = Get-ChildItem .\anthropic-ai-claude-code-2.1.88.tar.part* | Sort-Object Name
$output = [IO.File]::Create('.\anthropic-ai-claude-code-2.1.88.tar')
try {
  foreach ($part in $parts) {
    $input = [IO.File]::OpenRead($part.FullName)
    try { $input.CopyTo($output) } finally { $input.Dispose() }
  }
}
finally { $output.Dispose() }
Get-FileHash .\anthropic-ai-claude-code-2.1.88.tar -Algorithm SHA256
```

Expected SHA-256:

`7EFEC7B4166C948F9540E2A4448E656ACBB6595D8C7D832F3E96411DE76920EF`
