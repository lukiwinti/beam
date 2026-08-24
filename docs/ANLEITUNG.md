# Eigenständiger Windows-Sender für den Tesla-Browser

Der Windows-Sender ist eine einzelne Desktop-Anwendung. Er nimmt einen ausgewählten Windows-Bildschirm sowie den Windows-Systemton auf, kodiert das Bild als H.264 und stellt die Empfängerseite über einen eingebauten HTTP-/WebSocket-Server bereit. DNS und ein Internetzugang werden dafür nicht benötigt.

## Aufbau

```text
Windows-Bildschirm
  → Windows Graphics Capture
  → OpenH264 Baseline (Annex B)
  → eingebetteter WebSocket-Server
  → WebCodecs/H.264 oder HTTP-JPEG-Fallback + Canvas im Tesla-Browser

Windows-Standardausgabegerät
  → WASAPI Loopback (48 kHz, Stereo)
  → derselbe WebSocket und dieselbe Zeitbasis wie das Bild
  → Web Audio im Tesla-Browser
```

Alle Bestandteile laufen in `tesla-screen-sender.exe`. Ein echter Monitor und ein HDMI-Dummy-/Display-Emulator werden von Windows gleich behandelt, solange der Bildschirm in den Windows-Anzeigeeinstellungen aktiv ist.

## Voraussetzungen

- Windows 10 oder Windows 11
- Tesla und Windows-PC am selben Router/WLAN
- Ein aktueller Browser; bei HTTP wird automatisch der JPEG-Kompatibilitätsmodus verwendet
- Für den Selbstbau: aktuelle Rust-MSVC-Toolchain und Visual Studio Build Tools mit „Desktopentwicklung mit C++“

## Anwendung bauen

Im Repository in PowerShell:

```powershell
rustup default stable-x86_64-pc-windows-msvc
.\build-release.ps1
```

Die fertige Anwendung liegt anschließend unter:

```text
build\release\tesla-screen-sender.exe
```

Die EXE enthält auch HTML, CSS und JavaScript der Tesla-Empfängerseite. Es müssen keine Webdateien oder Serverprogramme daneben kopiert werden.

## Erster Start

1. `build\release\tesla-screen-sender.exe` starten.
2. „Einstellungen“ öffnen und den gewünschten Bildschirm wählen. Falls ein HDMI-Dummy erst später angeschlossen wurde, „Neu laden“ drücken.
3. Port, Bildrate und Bitrate einstellen. Für den ersten Test sind `8080`, `30 FPS` und `8000 kbit/s` sinnvoll. „Systemton übertragen“ aktiviert lassen.
4. Die voreingestellte PIN `123456` vor der Nutzung ändern.
5. „Stream starten“ drücken.
6. Falls Windows Defender Firewall fragt, den Zugriff im **privaten Netzwerk** erlauben. Keine Freigabe für öffentliche Netzwerke ist nötig.
7. Die in der App angezeigte Adresse, beispielsweise `http://192.168.10.177:8080`, im Tesla-Browser öffnen.
8. Die PIN aus der Windows-App eingeben und „Stream öffnen“ drücken.
9. Optional über die Schaltfläche oben rechts in den Browser-Vollbildmodus wechseln.

Die Einstellungen bleiben nach dem Beenden erhalten. Die Anwendung speichert sie unter `%LOCALAPPDATA%\TeslaScreenSender\settings.json` und lädt sie beim nächsten Start automatisch. Browser blockieren automatische Audiowiedergabe teilweise bis zur ersten Benutzergeste. Falls kein Ton hörbar ist, im Stream einmal die Schaltfläche `🔊` antippen.

### Ton und Synchronisation

Die Anwendung nimmt das Windows-Standardausgabegerät per WASAPI-Loopback mit 48 kHz in Stereo auf. Bild und Ton erhalten Zeitstempel aus derselben Uhr. Im Empfänger ist Audio die Referenz: Videoframes werden gegen die geplante Audiowiedergabe angezeigt, anstatt Ton und Bild mit zwei voneinander driftenden Timern abzuspielen. Wird das Windows-Standardausgabegerät während eines laufenden Streams gewechselt, den Stream einmal stoppen und neu starten.

### Hinweis zu HTTP und WebCodecs

Browser stellen `VideoDecoder` nur in einem sicheren HTTPS-Kontext oder für `localhost` bereit. Eine über `http://192.168.…` geöffnete Seite kann WebCodecs daher unabhängig vom verwendeten Browser nicht sehen. Die Anwendung erkennt das automatisch und wechselt auf einen JPEG-Stream, der ohne Zertifikat über die lokale IP funktioniert. Bei einem späteren Betrieb über vertrauenswürdiges HTTPS wird automatisch wieder der effizientere H.264-/WebCodecs-Pfad benutzt.

### HTTPS mit Hetzner DNS einrichten

1. Einen Hostnamen festlegen, beispielsweise `tesla.example.de`.
2. Sicherstellen, dass dieser Hostname im WLAN des Fahrzeugs auf die lokale IP des Windows-PCs auflöst. Das kann über den DNS-Server des Routers, einen lokalen DNS-Server oder einen passenden öffentlichen A-/AAAA-Eintrag erfolgen.
3. In der Hetzner Console das Projekt öffnen, in dem die DNS-Zone liegt.
4. Unter **Security → API Tokens** einen neuen Token mit Lese-/Schreibzugriff erzeugen und sofort kopieren. Ein reiner Read-only-Token kann den temporären TXT-Eintrag nicht anlegen. Bei migrierten DNS-Zonen muss ein Token aus der neuen Hetzner Console verwendet werden; alte Tokens aus der früheren DNS Console funktionieren mit der neuen API nicht.
5. In Tesla Screen Sender **Einstellungen → HTTPS und Let's Encrypt** öffnen.
6. HTTPS aktivieren, den Hostnamen ohne `https://` und ohne Port sowie eine gültige E-Mail-Adresse eintragen.
7. Als DNS-Provider **Hetzner** auswählen und den Token in **API-Token** einfügen.
8. Die Let's-Encrypt-Nutzungsbedingungen akzeptieren und die Einstellungen speichern.
9. **Stream starten**. Beim ersten Mal wird der geprüfte ACME-Client heruntergeladen, `_acme-challenge.<domain>` kurzzeitig über die Hetzner-API gesetzt und anschließend das Zertifikat angefordert. Das kann je nach DNS-Propagation einige Minuten dauern; die GUI bleibt dabei bedienbar.
10. Danach die angezeigte Adresse, beispielsweise `https://tesla.example.de:8080`, im Tesla öffnen.

Für die Ermittlung der DNS-Zone verwendet die Anwendung ausdrücklich die IPv4-Resolver `1.1.1.1` und `1.0.0.1`. Die eigentliche lokale Propagationsprüfung erfolgt direkt gegen die autoritativen Hetzner-Nameserver. Rekursive Resolver werden dafür nicht verwendet, weil sie einen zuvor nicht vorhandenen Challenge-Namen als `NXDOMAIN` zwischenspeichern können. Der HTTPS-Dienst selbst muss für DNS-01 zu keinem Zeitpunkt aus dem Internet erreichbar sein.

Die Anwendung prüft das Zertifikat alle zwölf Stunden. `lego` erneuert es 30 Tage vor Ablauf; anschließend wird das neue Zertifikat ohne Streamneustart geladen. Falls eine Erneuerung vorübergehend fehlschlägt, läuft das vorhandene gültige Zertifikat weiter und beim nächsten Intervall erfolgt ein neuer Versuch.

Direkt auswählbar sind außerdem Cloudflare, IONOS, Netcup, DigitalOcean, Duck DNS, deSEC.io, http.net, IPv64 und Vercel. Jeder Provider zeigt nur die für ihn benötigten Zugangsfelder an. Die zugrunde liegende Registry ist vom Server getrennt und kann um weitere der von `lego` angebotenen DNS-Provider erweitert werden.

### Speicherorte

- Allgemeine Einstellungen: `%LOCALAPPDATA%\TeslaScreenSender\settings.json`
- Mit Windows DPAPI verschlüsselte DNS-Zugangsdaten: `%LOCALAPPDATA%\TeslaScreenSender\acme-credentials.dpapi`
- ACME-Konto und Zertifikate: `%LOCALAPPDATA%\TeslaScreenSender\acme`
- Geprüfter ACME-Client: `%LOCALAPPDATA%\TeslaScreenSender\tools`

Die API-Zugangsdaten sind dadurch an den aktuellen Windows-Benutzer und denselben Computer gebunden. Das Kopieren der `.dpapi`-Datei auf einen anderen PC liefert keinen lesbaren API-Schlüssel.

### Einstellungen für Video

Für YouTube und andere Inhalte mit viel Bewegung sind bei einem 1920×1080-Bildschirm `30 FPS` und `8000–12000 kbit/s` ein guter Ausgangspunkt. Der native H.264-Pfad lässt keine Frames mehr zur Einhaltung einer starren Bitrate aus. Aufnahme und Browser arbeiten als Echtzeitpipeline: Bei einer kurzen Überlastung wird am nächsten Schlüsselbild resynchronisiert, anstatt einen immer älter werdenden Bildpuffer abzuarbeiten.

## Netzwerk

Die App bindet standardmäßig an `0.0.0.0` und ist damit über alle Netzwerkschnittstellen des PCs erreichbar. Es wird nur der konfigurierte TCP-Port benötigt. DNS ist nicht erforderlich; im Tesla wird die angezeigte IPv4-Adresse direkt geöffnet.

Im HTTP-Modus sind PIN und Videostream nicht verschlüsselt. Den HTTP-Port nicht ins Internet weiterleiten und diesen Modus nur in einem vertrauenswürdigen Fahrzeug-/Heimnetz verwenden. Im HTTPS-Modus werden Seite, Anmeldung, WebSocket, Bild und Ton TLS-verschlüsselt übertragen.

Wenn die Seite nicht erreichbar ist:

1. Prüfen, ob Tesla und PC IP-Adressen im gleichen Subnetz besitzen.
2. Im Windows-Netzwerkprofil „Privates Netzwerk“ verwenden.
3. Prüfen, ob die Firewall die EXE für private Netze zulässt.
4. Testweise die in der App angezeigte URL auf einem Smartphone im selben WLAN öffnen.
5. Sicherstellen, dass der Port nicht bereits von einer anderen Anwendung verwendet wird.

## Verhalten bei Unterbrechungen

Der Browser verbindet den WebSocket nach einer kurzen WLAN-Unterbrechung automatisch neu. Der Server hält das letzte H.264-Schlüsselbild vor, sodass ein neu verbundener Browser unmittelbar in den laufenden Stream einsteigen kann. Nach drei fehlgeschlagenen Verbindungsversuchen wird eine alte Browsersitzung verworfen und die PIN erneut abgefragt.

## Grenzen der ersten Version

- Der Browser ist nur Empfänger. Touch-, Maus- und Tastatureingaben werden nicht zurück an Windows gesendet.
- OpenH264 kodiert in dieser Version per CPU. 4K bei hoher Bildrate kann deshalb je nach Laptop zu langsam sein. Für 1920×1080 sind 30 FPS ein sinnvoller Startpunkt.
- OpenH264 unterstützt maximal 3840×2160 im Querformat beziehungsweise 2160×3840 im Hochformat.
- Die PIN gilt bis zum Stoppen des Streams. Beim nächsten Start wird eine neue Serversitzung mit neuem internem Zugriffstoken erzeugt.

## Sicherheits- und Fahrzeughinweis

Die PIN schützt vor zufälligem Zugriff anderer Geräte im WLAN, ersetzt aber wegen HTTP keine verschlüsselte Verbindung. Die Empfängerseite setzt restriktive Browser-Sicherheitsheader und speichert den Zugriffstoken nur für die aktuelle Browser-Sitzung.

Die Verwendung im Fahrzeug darf die sichere Bedienung nicht beeinträchtigen. Die Anwendung ist für stationäre Tests beziehungsweise den beschriebenen Prüfstand gedacht, nicht für die Bedienung während realer Straßenfahrt.
