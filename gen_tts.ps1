Add-Type -AssemblyName System.Speech
$s = New-Object System.Speech.Synthesis.SpeechSynthesizer
$text = "The quick brown fox jumps over the lazy dog. Speech recognition should transcribe this clearly and accurately."
Set-Content -Path .\reference.txt -Value $text -NoNewline
$s.Rate = 0
$s.SetOutputToWaveFile(".\speech.wav")
$s.Speak($text)
$s.Dispose()
Write-Host "done"
(Get-Item .\speech.wav).Length