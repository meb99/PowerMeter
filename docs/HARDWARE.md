# Hardware: Werte vom Labornetzteil holen

## Die kurze Antwort

Das **Wanptek DPS3010U** (30 V / 10 A) hat **keine Datenschnittstelle**. Die USB-Buchsen vorne (USB-A, je nach Version auch USB-C) sind reine 18-W-Schnellladeausgänge, dahinter sitzt nur ein Ladechip und kein USB-Seriell-Wandler. Es gibt kein USB-Daten, kein RS232, kein RS485, kein Modbus und keine PC-Software, und die LEDs für CV/CC/OCP sind nur Anzeigen. Auslesen lässt es sich also nicht direkt. Vorsicht bei Suchergebnissen zu „DPS5005“ oder „DPS3005“: Das sind Module eines anderen Herstellers (RDTech/Riden), deren Modbus-Protokoll gilt nicht für Wanptek.

**Es geht trotzdem**, und zwar besser als gedacht: Man setzt ein kleines **Messmodul zwischen Netzteil und Gerät**. Das misst Spannung und Strom am Ausgang direkt, 100-mal pro Sekunde und genauer als die Anzeige des Netzteils, und schickt die Werte per USB an die App.

```
 Wanptek  (+) ───► [INA228: V+ ─ Shunt ─ V−] ───► (+)  Gerät (Mainboard, Laptop …)
 DPS3010U (−) ──────────────────────────────────► (−)
                         │
                 INA228 + ESP32 ──USB──► Mac (PowerMeter-App → OBS)
```

Was das Messmodul **nicht** kann: die eingestellten Knopfwerte lesen (Soll-Spannung, Strombegrenzung). Darum kümmert sich die App:

- **Soll-Spannung** lernt sie automatisch, sobald der Ausgang ohne Last läuft (dann liegt genau die eingestellte Spannung an).
- **Strombegrenzung** lernt sie automatisch, sobald das Netzteil in die Begrenzung geht (Strom bleibt flach, Spannung bricht ein) oder bei einem Kurzschluss.
- Oder du trägst beides einmal rechts in der App ein.

Daraus erkennt die App **CV/CC**, **Spannungseinbrüche** und **Kurzschlüsse**.

## Einkaufsliste (Empfehlung, ca. 30–45 €)

| Teil | Wofür | ca. Preis |
|---|---|---|
| **ESP32-C3 SuperMini**, Variante „Soldered“ (oder ESP32-S3 / Raspberry Pi Pico) | Mikrocontroller mit USB direkt am Chip | 3–6 € |
| **Adafruit INA228** (Produkt #5832) | 20-Bit-Messchip für Spannung und Strom, bis 85 V, mit eingebautem 15-mΩ-Shunt, Schraubklemme und STEMMA-QT-Buchse | 12–16 € |
| **STEMMA-QT-Kabel** auf 4 Dupont-Buchsen | INA228 ↔ ESP32 ohne Löten | 1–3 € |
| 4 × **4-mm-Polklemmen/Bananenbuchsen** (2 × rot, 2 × schwarz) | Netzteil rein, Gerät raus | 2–4 € |
| Silikonkabel **1,0 mm² / AWG 18** (rot/schwarz) | Stromwege in der Box; 1,5 mm² passt nicht mehr gut in die 3,5-mm-Schraubklemme | 3–5 € |
| Dupont-Kabelset Buchse–Buchse | die zwei dünnen Kabel für VBus und GND | 1–2 € |
| 2 × Laborkabel Banane–Banane, 4 mm | Netzteil → Box | 3–6 € |
| Kleines Gehäuse (ca. 100 × 60 × 40 mm) | | 3–5 € |
| optional: **USB-Isolator ADuM3160** | Trennt Mac-Masse vom Messaufbau (siehe unten) | 8–12 € |

Der **eingebaute Shunt** (15 mΩ) reicht für bis zu ca. **5 A Dauerstrom**. Ein externer Shunt ist dafür **nicht** nötig. Er würde auf dem Adafruit-Board parallel zum eingebauten liegen und die Messung verfälschen, solange der eingebaute nicht ausgelötet ist (siehe „Mehr als 5 A“ unten).

**Auflösung:** etwa 0,2 mV bei der Spannung und etwa 0,02 mA beim Strom. Das ist deutlich feiner als die Anzeige des Netzteils (10 mV / 1 mA).

## Verdrahtung

Board so hinlegen: Bauteile nach oben, Schraubklemme zu dir hin. Dann ist an der Schraubklemme **links V+, Mitte VBus, rechts V−**. Der Aufdruck dazu steht auf der Rückseite des Boards.

| Von | Nach | Kabel |
|---|---|---|
| Polklemme + (vom Netzteil) | INA228 Schraube **V+** | dick, rot |
| INA228 Schraube **V−** | Polklemme + (zum Gerät) | dick, rot |
| Polklemme − (vom Netzteil) | Polklemme − (zum Gerät) | dick, schwarz |
| INA228 Schraube **VBus** | Polklemme + (zum Gerät) | dünn, rot |
| INA228 Stift **GND** (Stiftleiste anlöten) | Polklemme − (vom Netzteil) | dünn, schwarz |
| INA228 STEMMA QT | ESP32 **3.3** (rot), **G** (schwarz), **GPIO 8** (blau, SDA), **GPIO 9** (gelb, SCL) | STEMMA-QT-Kabel |
| ESP32 USB-C | Mac | USB-C-Datenkabel |

- **VBus** ist auf dem Adafruit-Board ab Werk nicht verbunden (Lötbrücke SJ1 offen). Das dünne Kabel zum Ausgang + misst die Spannung, die am Gerät ankommt.
- Die Stiftleiste liegt dem Board lose bei. Gebraucht wird nur der Stift **GND**, das ist der einzige Lötjob.
- SDA/SCL-Pins sind in der Firmware änderbar (`PIN_SDA`, `PIN_SCL`).
- Spannung vom Netzteil darf **nie** an einen Pin des ESP32 kommen, nur an die Schraubklemme des INA228.
- Strombegrenzung am Netzteil höchstens **5 A** einstellen. Das DPS3010U kann bis 10 A liefern. So stellst du die Grenze ein: Ausgang mit einem dicken Kabel direkt am Netzteil kurzschließen, Ausgang an, A-Regler drehen, bis höchstens 5,00 A angezeigt werden, Ausgang aus, Kabel ab.
- Die grüne **GND**-Klemme am Netzteil (Schutzerde) bleibt frei, die Box nutzt nur + und −.

> **Masse-Hinweis:** Über USB ist danach der Minuspol des Netzteils mit der Masse des Macs verbunden. Beim Wanptek ist der Ausgang normalerweise potentialfrei, dann ist das unkritisch. Prüfen: Netzstecker ziehen und mit dem Multimeter Durchgang zwischen **−** und der grünen **GND**-Klemme messen. Kein Durchgang heißt potentialfrei. Hängt am Prüfling aber gleichzeitig etwas Geerdetes (Oszilloskop, anderes Netzteil, geerdetes Gerät), nimm den **USB-Isolator** dazwischen. So vermeidest du Masseschleifen und schützt den Mac-Port.

### Mehr als 5 A (bis 10 A)

1. Den eingebauten Shunt **R1** vom INA228-Board auslöten.
2. Externen Shunt **10 A / 75 mV** in die Plus-Leitung setzen (Netzteil + → Shunt → Ausgang +, mit 1,5–2,5 mm²).
3. Zwei dünne Messkabel von den kleinen Schrauben des Shunts an **V+** (Netzteilseite) und **V−** (Geräteseite) der Schraubklemme.
4. VBus und GND bleiben wie oben.
5. In der Firmware `SHUNT_OHMS = 0.0075` eintragen.

Andere INA228-Boards (z. B. blaue „CJMCU-228“-Klone mit R010-Shunt) gehen auch. Dort `SHUNT_OHMS` auf `0.010` setzen und die Beschriftung des Boards prüfen.

## Firmware aufspielen

1. Arduino IDE installieren und den Boardsupport „esp32 by Espressif“ hinzufügen.
2. `firmware/powermon/powermon.ino` öffnen. `SHUNT_OHMS` steht schon auf `0.015` für das Adafruit-Board.
3. Board **ESP32C3 Dev Module** wählen und **USB CDC On Boot: Enabled** setzen. Dann hochladen. Klappt das nicht: **BOOT** gedrückt halten, USB einstecken, loslassen, nochmal hochladen.
4. Zum Test im seriellen Monitor schauen: Es sollten Zeilen wie `PM,123456,19.00012,0.000041` erscheinen.

## In der App

1. Oben bei **Netzteil** „PowerMon (INA228, USB)“ wählen, den Port (`cu.usbmodem…`) auswählen und auf **Verbinden** klicken.
2. Optional unter **⚙ Einstellungen → Kalibrierung**:
   - **Strom-Nullpunkt**: bei Ausgang ohne Last klicken.
   - **U an Multimeter angleichen**: das XDM1241 in V DC parallel an den Ausgang klemmen und klicken.
   - **I an Multimeter angleichen**: das XDM1241 in A DC in Reihe schalten und klicken.

   Damit liegt das Messmodul auf der Genauigkeit des XDM1241.

## OWON SPM6103 (oder andere SPM)

Die OWON-SPM-Netzteile (SPM3051, SPM6053, SPM3103, SPM6103) haben eine USB-Schnittstelle und ein eingebautes Multimeter (4½ Stellen). Die App unterstützt sie direkt. Die SPE-Modelle (gleiches Netzteil ohne Multimeter) und baugleiche „multicomp pro MP7111…“ gehen auch.

### Anschließen

1. USB-B-Kabel vom Netzteil (Rückseite) an den Mac. Meist ist kein Treiber nötig: Der USB-Chip ist sehr wahrscheinlich ein CH340, den aktuelle macOS-Versionen selbst kennen.
2. In der App oben bei **Netzteil** „OWON SPM (USB)“ wählen und den Port `cu.usbserial-…` auswählen, dann **Verbinden**. (Ist schon ein Port gespeichert, verbindet die App beim Wechsel sofort; ein anderer Port verbindet neu.)
3. Für das eingebaute Multimeter bei **Multimeter** „Im Netzteil (OWON SPM)“ wählen.

Die App probiert erst die eingestellte Baudrate (Standard 115200, ⚙ Einstellungen → OWON SPM) und, wenn keine Antwort kommt, die andere von 115200 und 9600. Klappt nur die andere, steht sie unten in der Statuszeile.

### Was es liefert

- Spannung, Strom und Leistung, etwa 10 Abfragen pro Sekunde.
- Die eingestellten Werte direkt vom Gerät: Sollspannung, Strombegrenzung, OVP und OCP. Drehen am Netzteil zeigt die App nach etwa einer halben Sekunde. Nichts muss gelernt oder eingetragen werden.
- CV/CC, Ausgang an/aus und Schutzabschaltungen (OVP, OCP, Übertemperatur). Eine Schutzabschaltung erscheint als Ereignis und als Warnung im Overlay.
- Steuerung rechts unter **Netzteil-Einstellung**: Werte eintragen und mit **Übernehmen** (oder Enter) ans Netzteil schicken. Die Knöpfe 3,3 / 5 / 12 / 19 / 20 V füllen nur das Feld aus. Der große Knopf schaltet den Ausgang, Taste `O` schaltet ihn sofort aus.
- Das Multimeter: V DC/AC, A DC/AC, Widerstand, Durchgang, Diode, Kapazität.

Die App schaltet den Ausgang nie von selbst ein. Beim Verbinden, Trennen oder Beenden bleibt der Ausgang, wie er ist, damit ein Board in der Reparatur nicht plötzlich ohne Strom dasteht. Unter ⚙ Einstellungen lässt sich zusätzlich eine **Spannungsgrenze** setzen, über die die App nie einstellt.

### Grenzen

- Auflösung 10 mV und 1 mA, Genauigkeit beim Strom etwa ±20 mA. Für Ruhe- und Standby-Ströme im mA-Bereich ist das zu grob.
- Das Netzteil erneuert seine eigenen Messwerte vermutlich nur etwa 3-mal pro Sekunde. Kurze Einbrüche (unter ca. 300 ms) sieht man damit nicht.
- Das Multimeter hat keinen µA-Bereich (A DC: 200 mA und 10 A) und keine Frequenz, Periode oder Temperatur. Ein Wechsel der Messfunktion dauert knapp eine Sekunde.
- Die Kalibrierung unter ⚙ Einstellungen gilt nur für die PowerMon-Box, nicht für das SPM.

### Empfohlen: SPM und PowerMon-Box zusammen

Die Box misst 100-mal pro Sekunde mit etwa 0,02 mA Auflösung, das SPM liefert Sollwerte, CV/CC, Steuerung und das Multimeter. Dafür die Box wie oben zwischen SPM und Gerät setzen, beide per USB an den Mac, und unter ⚙ Einstellungen → OWON SPM **Messwerte von der PowerMon-Box (100 Hz)** einschalten und den Port der Box wählen. Bei **Netzteil** bleibt „OWON SPM (USB)“ ausgewählt.

### Erst einmal testen

Bevor du dich auf das Gerät verlässt, im Terminal einmal lesen lassen (verstellt nichts, schickt nur Abfragen):

```sh
/Applications/PowerMeter.app/Contents/MacOS/PowerMeter --probe-spm /dev/cu.usbserial-1410   # Port anpassen; optional die Baudrate dahinter, z. B. 9600
```

Aus dem Quellcode geht dasselbe mit `cargo run --release -- --probe-spm /dev/cu.usbserial-1410`.

Die Ausgabe zeigt das Modell, 50 Messungen mit Zeitstempel und wie viele davon neue Werte waren (also wie oft das Netzteil wirklich misst), dazu Sollwerte, OVP/OCP, Ausgang und Multimeter. Für eine aussagekräftige Messrate eine Last anschließen, deren Strom sich bewegt.

## Alternativen

- **OWON SPM** statt des DPS3010U: siehe oben, wird direkt unterstützt.
- **Andere Netzteile mit Schnittstelle**: z. B. Hanmatek **HM310P** (USB, Modbus RTU) oder Riden **RD6012/RD6018** (USB/WLAN, Modbus). Die liefern Sollwerte auch direkt, dafür braucht die App aber noch einen eigenen Treiber (lässt sich nachrüsten).
- **Kamera auf das Display**: In OBS sowieso möglich, aber ohne Graphen, Ereignisse und Genauigkeit.
- Ein beliebiges anderes Messmodul geht auch, solange es pro Zeile `Spannung,Strom` über USB ausgibt. Die App versteht das Format direkt.
