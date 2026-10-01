# Hardware: Werte vom Labornetzteil holen

## Die kurze Antwort

Das **Wanptek GA3010H** (30 V / 10 A) hat **keine Datenschnittstelle**. Die USB-A- und USB-C-Buchsen vorne sind reine Ladebuchsen, und „programmable“ bezieht sich nur auf die Speicherplätze M1–M3. Es gibt kein USB-Daten, kein RS232, kein RS485 und kein Modbus. Auslesen lässt es sich also nicht direkt.

**Es geht trotzdem**, und zwar besser als gedacht: Man setzt ein kleines **Messmodul zwischen Netzteil und Gerät**. Das misst Spannung und Strom am Ausgang direkt, 100-mal pro Sekunde und genauer als die Anzeige des Netzteils, und schickt die Werte per USB an die App.

```
 Wanptek  (+) ──────► [Shunt]──────► (+)  Gerät (Mainboard, Laptop …)
 GA3010H  (−) ─────────┬───────────► (−)
                       │
                 INA228 + ESP32 ──USB──► Mac (PowerMeter-App → OBS)
```

Was das Messmodul **nicht** kann: die eingestellten Knopfwerte lesen (Soll-Spannung, Strombegrenzung). Darum kümmert sich die App:

- **Soll-Spannung** lernt sie automatisch, sobald der Ausgang ohne Last läuft (dann liegt genau die eingestellte Spannung an).
- **Strombegrenzung** lernt sie automatisch, sobald das Netzteil in die Begrenzung geht (Strom bleibt flach, Spannung bricht ein) oder bei einem Kurzschluss.
- Oder du trägst beides einmal rechts in der App ein.

Daraus erkennt die App **CV/CC**, **Spannungseinbrüche** und **Kurzschlüsse**.

## Einkaufsliste (Empfehlung, ca. 25–35 €)

| Teil | Wofür | ca. Preis |
|---|---|---|
| **ESP32-C3 SuperMini** (oder ESP32-S3 / Raspberry Pi Pico) | Mikrocontroller mit USB direkt am Chip | 3–6 € |
| **INA228-Modul** (Adafruit #5832 oder „CJMCU-228“-Klon) | 20-Bit-Messchip für Spannung und Strom, bis 85 V | 5–15 € |
| **Shunt 10 A / 75 mV** (z. B. „FL-2 10A 75mV“) | Messwiderstand für volle 10 A | 4–8 € |
| 4 × **4-mm-Polklemmen/Bananenbuchsen** (2 × rot, 2 × schwarz) | Netzteil rein, Gerät raus | 4–6 € |
| Silikonkabel **1,5 mm²** (rot/schwarz), Dupont-Kabel | Starkstrom- und Signalwege | 3–5 € |
| Kleines Gehäuse | | 3–5 € |
| optional: **USB-Isolator ADuM3160** | Trennt Mac-Masse vom Messaufbau (siehe unten) | 8–12 € |

**Nur bis ca. 5 A?** Dann reicht der Shunt, der schon auf dem INA228-Board sitzt. Der externe Shunt entfällt, du trägst nur dessen Wert in der Firmware ein (Adafruit: 15 mΩ, blaue Klone: meist R010 = 10 mΩ).

**Auflösung mit dem 75-mV-Shunt:** etwa 0,2 mV bei der Spannung und etwa 0,04 mA beim Strom. Das ist feiner als die Anzeige des Netzteils (10 mV / 10 mA).

## Verdrahtung

1. **Netzteil +** → Shunt-Anschluss A. Shunt-Anschluss B → **Ausgang +** (Gerät).
2. **Netzteil −** → **Ausgang −**, durchgehend mit dickem Kabel.
3. INA228 **IN+** an Shunt-Seite A, **IN−** an Shunt-Seite B. Die Messleitungen direkt an die kleinen Schrauben des Shunts führen, nicht an die dicken Strombolzen.
4. INA228 **VBUS** an **IN−**. Damit wird die Spannung gemessen, die am Gerät ankommt. Viele Boards haben das schon per Lötbrücke verbunden.
5. INA228 **GND** an **Netzteil −**, **VCC** an **3V3** vom ESP32.
6. **SDA → GPIO 8**, **SCL → GPIO 9** (ESP32-C3 SuperMini, in der Firmware änderbar).
7. ESP32 per USB-C an den Mac.

> **Masse-Hinweis:** Über USB ist danach der Minuspol des Netzteils mit der Masse des Macs verbunden. Beim Wanptek (Ausgang potentialfrei) ist das unkritisch. Hängt am Prüfling aber gleichzeitig etwas Geerdetes (Oszilloskop, anderes Netzteil, geerdetes Gerät), nimm den **USB-Isolator** dazwischen. So vermeidest du Masseschleifen und schützt den Mac-Port.

## Firmware aufspielen

1. Arduino IDE installieren und den Boardsupport „esp32 by Espressif“ hinzufügen.
2. `firmware/powermon/powermon.ino` öffnen.
3. Oben `SHUNT_OHMS` an deinen Shunt anpassen (`0.0075` für 10 A / 75 mV).
4. Board **ESP32C3 Dev Module** wählen und **USB CDC On Boot: Enabled** setzen. Dann hochladen.
5. Zum Test im seriellen Monitor schauen: Es sollten Zeilen wie `PM,123456,19.00012,0.000041` erscheinen.

## In der App

1. Oben bei **Netzteil** „PowerMon (INA228, USB)“ wählen, den Port (`cu.usbmodem…`) auswählen und auf **Verbinden** klicken.
2. Optional unter **⚙ Einstellungen → Kalibrierung**:
   - **Strom-Nullpunkt**: bei Ausgang ohne Last klicken.
   - **U an Multimeter angleichen**: das XDM1241 in V DC parallel an den Ausgang klemmen und klicken.
   - **I an Multimeter angleichen**: das XDM1241 in A DC in Reihe schalten und klicken.

   Damit liegt das Messmodul auf der Genauigkeit des XDM1241.

## Alternativen

- **Netzteil mit Schnittstelle**: Ein Riden **RD6012/RD6018** (USB/WLAN, Modbus) liefert Soll-Werte und CV/CC direkt. Das kostet aber deutlich mehr und braucht noch einen Treiber in der App (lässt sich nachrüsten).
- **Kamera auf das Display**: In OBS sowieso möglich, aber ohne Graphen, Ereignisse und Genauigkeit.
- Ein beliebiges anderes Messmodul geht auch, solange es pro Zeile `Spannung,Strom` über USB ausgibt. Die App versteht das Format direkt.
