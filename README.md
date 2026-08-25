# Tesla Screen Sender

Eigenständige Windows-Anwendung zur Übertragung eines ausgewählten Bildschirms an den Tesla-Browser im lokalen Netzwerk.

Die Anwendung enthält alles in einem Prozess:

- vollständige Windows-GUI mit Monitorauswahl
- Windows Graphics Capture
- WASAPI-Loopback-Aufnahme des Windows-Systemtons mit gemeinsamer A/V-Zeitbasis
- optionaler Rückkanal für native Windows-Touchbedienung und Tesla-Bildschirmtastatur
- H.264-Encoding mit OpenH264
- lokalen HTTP-/WebSocket-Server
- PIN-Anmeldung
- touchfreundliche Tesla-Empfängerseite mit WebCodecs und automatischem HTTP-JPEG-Fallback
- zeitlich unbegrenzte automatische Wiederverbindung nach WLAN- und Serverunterbrechungen
- für Vollbewegung optimierter H.264-Pfad ohne encoderseitiges Frame-Skipping oder wachsenden Browserpuffer
- persistentes Einstellungsfenster; Speicherung unter `%LOCALAPPDATA%\TeslaScreenSender\settings.json`
- optionalen HTTPS-Betrieb mit automatischer DNS-01-Challenge und Zertifikatserneuerung
- automatische HTTP-zu-HTTPS-Umleitung auf demselben Port für alte Browser-Lesezeichen
- Anzeige der tatsächlich kodierten Bildrate zusätzlich zur eingestellten Obergrenze

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
2. Über „Einstellungen“ Bildschirm, Port, Bildrate, Bitrate, PIN und Systemton auswählen.
   Dort kann „Bedienung übertragen“ separat ein- oder ausgeschaltet werden; die sichere Vorgabe ist aus.
3. „Stream starten“ drücken.
4. Den Firewallzugriff für private Netzwerke erlauben.
5. Die in der App angezeigte Adresse im Tesla-Browser öffnen.
6. PIN eingeben und den Stream öffnen.

Die ausführliche Anleitung steht unter [docs/ANLEITUNG.md](docs/ANLEITUNG.md).

## Aktueller Umfang

Diese Version überträgt Bild und Windows-Systemton. Touch-/Tastatursteuerung von Windows ist noch nicht enthalten.

Browser dürfen Audio häufig erst nach einer Benutzergeste wiedergeben. Falls der Ton nicht automatisch startet, im Stream einmal die Schaltfläche `🔊` antippen.

## HTTPS und Let's Encrypt

Im Einstellungsfenster kann zwischen HTTP und HTTPS gewechselt werden. Für HTTPS werden Domain, Let's-Encrypt-E-Mail, DNS-Provider und dessen API-Zugangsdaten eingetragen. Aktuell sind Hetzner, Cloudflare, IONOS, Netcup, DigitalOcean, Duck DNS, deSEC.io, http.net, IPv64 und Vercel direkt auswählbar. Die Provider-Registry nutzt den ACME-Client [lego](https://go-acme.github.io/lego/), der weitere DNS-Provider unterstützt und leicht ergänzt werden kann.

Der API-Schlüssel wird nicht in `settings.json` gespeichert, sondern mit Windows DPAPI benutzer- und rechnergebunden verschlüsselt. Beim ersten HTTPS-Start lädt die Anwendung die festgelegte Windows-Version von `lego` von dessen offizieller GitHub-Veröffentlichung und prüft sowohl Archiv als auch EXE per SHA-256. Danach wird das Zertifikat alle zwölf Stunden geprüft und bereits 30 Tage vor Ablauf erneuert. Ein erneuertes Zertifikat wird ohne Neustart des Streams in den HTTPS-Server geladen.

Die gewünschte Domain muss aus dem Tesla-Netz auf die lokale IP-Adresse des Windows-PCs auflösen. Die DNS-01-Challenge stellt nur das Zertifikat aus; sie legt bewusst keinen A-/AAAA-Eintrag für den PC an.

Beim Aufruf über eine normale lokale HTTP-IP verwendet die Seite automatisch den JPEG-Kompatibilitätsmodus. WebCodecs steht laut Browserstandard nur in sicheren HTTPS-Kontexten oder auf `localhost` zur Verfügung. Eine Zertifikats- oder DNS-Einrichtung ist für den JPEG-Modus nicht erforderlich.

Für Video über den nativen HTTPS-/WebCodecs-Modus sind bei 1920×1080 zunächst `30 FPS` und `8000–12000 kbit/s` empfehlenswert. Der Empfänger hält immer das aktuellste Bild; wenn Netzwerk oder Decoder kurzzeitig nicht nachkommen, wird am nächsten Schlüsselbild live resynchronisiert, statt alte Frames verzögert abzuspielen.
