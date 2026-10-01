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

## Alternativen

- **Netzteil mit Schnittstelle** (statt des DPS3010U): z. B. Hanmatek **HM310P** (USB, Modbus RTU) oder Riden **RD6012/RD6018** (USB/WLAN, Modbus). Die liefern Sollwerte direkt. Dafür braucht die App noch einen eigenen Treiber (lässt sich nachrüsten).
- **Kamera auf das Display**: In OBS sowieso möglich, aber ohne Graphen, Ereignisse und Genauigkeit.
- Ein beliebiges anderes Messmodul geht auch, solange es pro Zeile `Spannung,Strom` über USB ausgibt. Die App versteht das Format direkt.
