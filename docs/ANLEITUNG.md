# Eigenständiger Windows-Sender für den Tesla-Browser

Der Windows-Sender ist eine einzelne Desktop-Anwendung. Er nimmt einen ausgewählten Windows-Bildschirm auf, kodiert das Bild als H.264 und stellt die Empfängerseite über einen eingebauten HTTP-/WebSocket-Server bereit. DNS und ein Internetzugang werden dafür nicht benötigt.

## Aufbau

```text
Windows-Bildschirm
  → Windows Graphics Capture
  → OpenH264 Baseline (Annex B)
  → eingebetteter WebSocket-Server
  → WebCodecs + Canvas im Tesla-Browser
```

Alle Bestandteile laufen in `tesla-screen-sender.exe`. Ein echter Monitor und ein HDMI-Dummy-/Display-Emulator werden von Windows gleich behandelt, solange der Bildschirm in den Windows-Anzeigeeinstellungen aktiv ist.

## Voraussetzungen

- Windows 10 oder Windows 11
- Tesla und Windows-PC am selben Router/WLAN
- Ein Browser mit WebCodecs-Unterstützung
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
2. Den gewünschten Bildschirm wählen. Falls ein HDMI-Dummy erst später angeschlossen wurde, „Neu laden“ drücken.
3. Port, Bildrate und Bitrate einstellen. Für den ersten Test sind `8080`, `30 FPS` und `8000 kbit/s` sinnvoll.
4. Die voreingestellte PIN `123456` vor der Nutzung ändern.
5. „Stream starten“ drücken.
6. Falls Windows Defender Firewall fragt, den Zugriff im **privaten Netzwerk** erlauben. Keine Freigabe für öffentliche Netzwerke ist nötig.
7. Die in der App angezeigte Adresse, beispielsweise `http://192.168.10.177:8080`, im Tesla-Browser öffnen.
8. Die PIN aus der Windows-App eingeben und „Stream öffnen“ drücken.
9. Optional über die Schaltfläche oben rechts in den Browser-Vollbildmodus wechseln.

## Netzwerk

Die App bindet standardmäßig an `0.0.0.0` und ist damit über alle Netzwerkschnittstellen des PCs erreichbar. Es wird nur der konfigurierte TCP-Port benötigt. DNS ist nicht erforderlich; im Tesla wird die angezeigte IPv4-Adresse direkt geöffnet.

Die Verbindung nutzt im lokalen Netz absichtlich HTTP. PIN und Videostream sind daher nicht verschlüsselt. Den Port nicht ins Internet weiterleiten und die Anwendung nur in einem vertrauenswürdigen Fahrzeug-/Heimnetz verwenden. Für nicht vertrauenswürdige Netze wäre ein vorgeschaltetes HTTPS-Konzept nötig.

Wenn die Seite nicht erreichbar ist:

1. Prüfen, ob Tesla und PC IP-Adressen im gleichen Subnetz besitzen.
2. Im Windows-Netzwerkprofil „Privates Netzwerk“ verwenden.
3. Prüfen, ob die Firewall die EXE für private Netze zulässt.
4. Testweise die in der App angezeigte URL auf einem Smartphone im selben WLAN öffnen.
5. Sicherstellen, dass der Port nicht bereits von einer anderen Anwendung verwendet wird.

## Verhalten bei Unterbrechungen

Der Browser verbindet den WebSocket nach einer kurzen WLAN-Unterbrechung automatisch neu. Der Server hält das letzte H.264-Schlüsselbild vor, sodass ein neu verbundener Browser unmittelbar in den laufenden Stream einsteigen kann. Nach drei fehlgeschlagenen Verbindungsversuchen wird eine alte Browsersitzung verworfen und die PIN erneut abgefragt.

## Grenzen der ersten Version

- Es wird nur das Bild übertragen; Systemton ist noch nicht enthalten.
- Der Browser ist nur Empfänger. Touch-, Maus- und Tastatureingaben werden nicht zurück an Windows gesendet.
- OpenH264 kodiert in dieser Version per CPU. 4K bei hoher Bildrate kann deshalb je nach Laptop zu langsam sein. Für 1920×1080 sind 30 FPS ein sinnvoller Startpunkt.
- OpenH264 unterstützt maximal 3840×2160 im Querformat beziehungsweise 2160×3840 im Hochformat.
- Die PIN gilt bis zum Stoppen des Streams. Beim nächsten Start wird eine neue Serversitzung mit neuem internem Zugriffstoken erzeugt.

## Sicherheits- und Fahrzeughinweis

Die PIN schützt vor zufälligem Zugriff anderer Geräte im WLAN, ersetzt aber wegen HTTP keine verschlüsselte Verbindung. Die Empfängerseite setzt restriktive Browser-Sicherheitsheader und speichert den Zugriffstoken nur für die aktuelle Browser-Sitzung.

Die Verwendung im Fahrzeug darf die sichere Bedienung nicht beeinträchtigen. Die Anwendung ist für stationäre Tests beziehungsweise den beschriebenen Prüfstand gedacht, nicht für die Bedienung während realer Straßenfahrt.
