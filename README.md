# Tesla Screen Sender

Eigenständige Windows-Anwendung zur Übertragung eines ausgewählten Bildschirms an den Tesla-Browser im lokalen Netzwerk.

Die Anwendung enthält alles in einem Prozess:

- vollständige Windows-GUI mit Monitorauswahl
- Windows Graphics Capture
- H.264-Encoding mit OpenH264
- lokalen HTTP-/WebSocket-Server
- PIN-Anmeldung
- touchfreundliche Tesla-Empfängerseite mit WebCodecs und Canvas
- automatische Wiederverbindung nach kurzen WLAN-Unterbrechungen

## Release bauen

```powershell
.\build-release.ps1
```

Die fertige Anwendung liegt danach hier:

```text
build\release\tesla-screen-sender.exe
```

Alternativ kann direkt mit Cargo gebaut werden:

```powershell
$env:CARGO_TARGET_DIR = "$PWD\build"
cargo build --release
```

## Benutzung

1. `build\release\tesla-screen-sender.exe` starten.
2. Bildschirm, Port, Bildrate, Bitrate und PIN auswählen.
3. „Stream starten“ drücken.
4. Den Firewallzugriff für private Netzwerke erlauben.
5. Die in der App angezeigte Adresse im Tesla-Browser öffnen.
6. PIN eingeben und den Stream öffnen.

Die ausführliche Anleitung steht unter [docs/ANLEITUNG.md](docs/ANLEITUNG.md).

## Aktueller Umfang

Diese Version überträgt das Bild. Audio sowie Touch-/Tastatursteuerung von Windows sind noch nicht enthalten.
