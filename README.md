# ⚡ PowerMeter

Live-Messwerte von **Labornetzteil** und **Multimeter** auf dem Mac, mit Graphen, CV/CC-Erkennung, Spannungseinbrüchen und einem **OBS-Overlay** für Reparaturvideos.

![App](docs/img/app.png)

## Was es kann

- **Netzteil-Werte live**: Spannung, Strom und Leistung mit 100 Messungen pro Sekunde, über das Messmodul zwischen Netzteil und Gerät ([Hardware & Einkaufsliste](docs/HARDWARE.md)).
- **OWON SPM** (SPM3051/6053/3103/6103, auch SPE und baugleiche multicomp-pro-Geräte) direkt über USB: Spannung, Strom und Leistung, die eingestellten Sollwerte, CV/CC und Schutzabschaltungen (OVP/OCP/OTP) kommen vom Netzteil selbst. Sollspannung, Strombegrenzung, OVP/OCP und den Ausgang stellst du aus der App, Taste `O` schaltet den Ausgang sofort aus. Das eingebaute Multimeter lässt sich als Multimeter der App nutzen. Mit der PowerMon-Box zusammen liefert die Box die schnellen Messwerte (100 Hz) und das SPM den Rest ([Details](docs/HARDWARE.md#owon-spm6103-oder-andere-spm)).
- **OWON XDM1241** (auch XDM1041/2041) über USB/SCPI. Messfunktion und Messrate lassen sich aus der App umschalten, und Drehen am Gerät erkennt die App selbst.
- **Graphen** für U, I, P und das Multimeter mit gemeinsamer Zeitachse. Zeitfenster von 5 s bis „Alles“. Spitzen und Einbrüche gehen auch bei langen Zeitfenstern nicht verloren (Min/Max-Ausdünnung).
- **CV / CC** als Anzeige. Beim OWON SPM kommen Sollwerte und CV/CC vom Netzteil. Sonst lernt die App die Soll-Spannung und die Strombegrenzung automatisch (im Overlay mit „≈“ markiert) oder du trägst sie ein.
- **Ereignisse**: Spannungseinbrüche, Strombegrenzung und Kurzschlüsse werden im Graphen farbig hinterlegt und aufgelistet. Ein Klick auf ein Ereignis springt im Graphen dorthin.
- **Energie & Ladung** (Wh, mAh), Min/Ø/Max, Welligkeit (Vpp) und Spitzenstrom.
- **Marker** (Taste `M`) markieren Momente für den Videoschnitt.
- **OBS-Overlay** als Browser-Quelle mit transparentem Hintergrund, Kachel oder Leiste, inklusive Mini-Graph und Kurzschluss-/Einbruch-Warnung.
- **Kalibrierung** des Messmoduls gegen das Multimeter per Knopfdruck.
- **CSV-Export** (Taste `E`) nach `~/Documents/PowerMeter/`.
- **Simulator** für Netzteil und Multimeter: Alles lässt sich ohne Hardware ausprobieren. „OWON SPM (Simulator)“ spielt ein SPM6103 nach, über denselben Treiber wie das echte Gerät.

## OBS einrichten

1. App starten. Oben steht die Overlay-URL `http://127.0.0.1:8765/overlay` (📋 kopiert sie).
2. In OBS: **Quellen → + → Browser**, die URL einfügen, Breite 700 und Höhe 400 eintragen.
3. Unter `http://127.0.0.1:8765/` gibt es eine Vorschau aller Varianten.

| Kachel | Leiste |
|---|---|
| ![Kachel](docs/img/overlay-card.png) | ![Leiste](docs/img/overlay-bar.png) |

Die URL-Parameter lassen sich kombinieren:

| Parameter | Werte | Standard |
|---|---|---|
| `show` | `v,i,p,mode,set,graph,dmm,energy,events` | `v,i,p,mode,set,graph,dmm` |
| `layout` | `card`, `bar` | `card` |
| `graph` | Kurven im Mini-Graph: `v,i,p` | `v,i` |
| `accent` | größter Wert: `v`, `i`, `p` | `v` |
| `scale` | Größe, z. B. `1.5` | `1` |
| `bg` | Hintergrund-Deckkraft `0`–`1` | `0.72` |

Beispiel nur für das Multimeter, groß: `http://127.0.0.1:8765/overlay?show=dmm&scale=1.4`

Die Daten kommen per Server-Sent-Events mit bis zu 60 Aktualisierungen pro Sekunde. Rohdaten als JSON liefert `/api/live`.

## Latenz & Genauigkeit

- Das Messmodul (INA228, 20 Bit) mittelt intern 16 Messungen und schickt alle 10 ms einen Wert. Die App zeichnet ihn sofort im nächsten Frame.
- Die **großen Zahlen** mitteln standardmäßig über 100 ms, damit die letzte Stelle im Video lesbar bleibt. Unter ⚙ lässt sich das auf 0 (roh) stellen. **Graphen und Ereignisse arbeiten immer mit den Rohdaten.**
- Das Multimeter wird so schnell abgefragt, wie es antwortet (Messrate „Schnell“). Die App zeigt 5 signifikante Stellen (passend zu den 55 000 Counts des XDM1241), unter ⚙ einstellbar.

## Bauen

Voraussetzung: [Rust](https://rustup.rs).

```sh
cargo run --release              # starten
./scripts/bundle_macos.sh        # dist/PowerMeter.app (Apple Silicon + Intel)
```

Beim ersten Start einer selbst gebauten App: Rechtsklick → **Öffnen**. Jeder Push baut die App auch auf GitHub Actions (Artefakt „PowerMeter-macOS“).

Ein OWON SPM einmal testen, ohne Fenster und ohne etwas am Netzteil zu verstellen (es wird nur gelesen):

```sh
cargo run --release -- --probe-spm /dev/cu.usbserial-1410        # Port anpassen, optional Baudrate dahinter
cargo run --release -- --probe-spm sim                           # gegen den Simulator
```

Die Ausgabe zeigt die Antwort auf `*IDN?`, 50 Messungen mit Zeitstempel (daraus: wie oft das Netzteil wirklich neue Werte liefert), die Sollwerte, den Ausgang und das Multimeter.

## Tastenkürzel

| Taste | Aktion |
|---|---|
| Leertaste | Graph anhalten / weiter (angehalten: ziehen und zoomen) |
| `M` | Marker setzen |
| `R` | Statistik & Energie zurücksetzen |
| `E` | CSV-Export |
| `O` | Ausgang **aus** (nur OWON SPM). Zum Einschalten gibt es absichtlich keine Taste. |

## Aufbau

```
src/
  main.rs            Fenster
  app.rs             Oberfläche (egui)
  model.rs           Messdaten, Analyse (CV/CC, Einbrüche, Energie)
  overlay.rs         Webserver für OBS (axum, SSE)
  export.rs          CSV
  devices/
    mod.rs           Gerätearten, SCPI-Verbindung (Pausen, Wiederholung)
    owon.rs          OWON XDM (SCPI über USB-Seriell)
    owon_spm.rs      OWON SPM: Netzteil + eingebautes Multimeter, Probe-Modus
    powermon.rs      Messmodul (INA228) über USB
    sim.rs           Simulatoren (auch ein SPM auf SCPI-Ebene)
assets/              Overlay-HTML
firmware/powermon/   Arduino-Firmware fürs Messmodul
docs/HARDWARE.md     Einkaufsliste & Verdrahtung
```

Inspiriert von [rusty_meter](https://github.com/markusdd/rusty_meter).
