// PowerMon – in-line DC power meter for the PowerMeter app.
//
// Hardware: any Arduino-compatible board with native USB (ESP32-C3/S3,
// Raspberry Pi Pico, ...) + INA228 breakout (Adafruit #5832 recommended).
// Wiring and parts list: docs/HARDWARE.md
//
// Streams one line per sample over USB:
//   PM,<micros>,<volts>,<amps>
// Commands (send a line):
//   INFO   prints the configuration as a '#' line
//   ZERO   takes the current with nothing connected as zero offset
//   RATE n sets the sample rate in Hz (1..500)

#include <Wire.h>

// ---------------------------------------------------------------- settings --

// Shunt resistance in ohms.
//   Adafruit INA228 board, onboard shunt (15 mΩ, up to ~5 A)  -> 0.015
//   R010 shunt on blue clone boards                           -> 0.010
//   external 10 A / 75 mV shunt                               -> 0.0075
//     (only with the onboard shunt removed, otherwise both are in parallel)
const float SHUNT_OHMS = 0.015f;

const uint8_t INA_ADDR = 0x40;  // A0 = A1 = GND

// I2C pins (ESP32 only; other boards use their default Wire pins).
// ESP32-C3 SuperMini: SDA = 8, SCL = 9
#ifndef PIN_SDA
#define PIN_SDA 8
#endif
#ifndef PIN_SCL
#define PIN_SCL 9
#endif

uint32_t sampleHz = 100;

// ------------------------------------------------------------ INA228 regs --

const uint8_t REG_CONFIG = 0x00;
const uint8_t REG_ADC_CONFIG = 0x01;
const uint8_t REG_VSHUNT = 0x04;
const uint8_t REG_VBUS = 0x05;
const uint8_t REG_MANUFACTURER_ID = 0x3E;
const uint8_t REG_DEVICE_ID = 0x3F;

// ADC_CONFIG: continuous shunt + bus (MODE = 0xB), 280 µs conversion for bus
// and shunt, 16x averaging -> a fresh, well-filtered value every ~9 ms.
const uint16_t ADC_CONFIG_VALUE = (0xB << 12) | (3 << 9) | (3 << 6) | (0 << 3) | 2;

const float VSHUNT_LSB = 312.5e-9f;    // ADCRANGE = 0 (±163.84 mV)
const float VBUS_LSB = 195.3125e-6f;

float currentOffset = 0.0f;
uint32_t nextSampleUs = 0;
String rxLine;

void writeReg16(uint8_t reg, uint16_t value) {
  Wire.beginTransmission(INA_ADDR);
  Wire.write(reg);
  Wire.write(value >> 8);
  Wire.write(value & 0xFF);
  Wire.endTransmission();
}

uint32_t readReg(uint8_t reg, uint8_t bytes) {
  Wire.beginTransmission(INA_ADDR);
  Wire.write(reg);
  if (Wire.endTransmission(false) != 0) return 0;
  Wire.requestFrom(INA_ADDR, bytes);
  uint32_t v = 0;
  for (uint8_t k = 0; k < bytes && Wire.available(); k++) v = (v << 8) | Wire.read();
  return v;
}

// 24-bit register holding a 20-bit two's complement value in bits 23..4.
int32_t read20(uint8_t reg) {
  uint32_t raw = readReg(reg, 3);
  return ((int32_t)(raw << 8)) >> 12;
}

bool inaPresent() {
  return readReg(REG_MANUFACTURER_ID, 2) == 0x5449 && (readReg(REG_DEVICE_ID, 2) >> 4) == 0x228;
}

void inaSetup() {
  writeReg16(REG_CONFIG, 0x8000);  // reset
  delay(5);
  writeReg16(REG_CONFIG, 0x0000);  // ADCRANGE = ±163.84 mV
  writeReg16(REG_ADC_CONFIG, ADC_CONFIG_VALUE);
}

float readVolts() { return read20(REG_VBUS) * VBUS_LSB; }

float readAmps() { return read20(REG_VSHUNT) * VSHUNT_LSB / SHUNT_OHMS - currentOffset; }

void printInfo() {
  Serial.print("# PowerMon INA228 shunt=");
  Serial.print(SHUNT_OHMS * 1000.0f, 3);
  Serial.print("mOhm rate=");
  Serial.print(sampleHz);
  Serial.print("Hz");
  if (!inaPresent()) Serial.print(" FEHLER: INA228 nicht gefunden");
  Serial.println();
}

void handleCommand(String cmd) {
  cmd.trim();
  cmd.toUpperCase();
  if (cmd == "INFO") {
    printInfo();
  } else if (cmd == "ZERO") {
    float sum = 0;
    currentOffset = 0;
    for (int k = 0; k < 64; k++) {
      sum += readAmps();
      delay(10);
    }
    currentOffset = sum / 64.0f;
    Serial.print("# zero offset ");
    Serial.print(currentOffset * 1000.0f, 4);
    Serial.println(" mA");
  } else if (cmd.startsWith("RATE ")) {
    long hz = cmd.substring(5).toInt();
    if (hz >= 1 && hz <= 500) sampleHz = hz;
    printInfo();
  }
}

void setup() {
  Serial.begin(115200);
#if defined(ARDUINO_ARCH_ESP32)
  Wire.begin(PIN_SDA, PIN_SCL);
#else
  Wire.begin();
#endif
  Wire.setClock(400000);
  inaSetup();
  delay(20);
  printInfo();
  nextSampleUs = micros();
}

void loop() {
  while (Serial.available()) {
    char c = Serial.read();
    if (c == '\n') {
      handleCommand(rxLine);
      rxLine = "";
    } else if (rxLine.length() < 64) {
      rxLine += c;
    }
  }

  uint32_t now = micros();
  if ((int32_t)(now - nextSampleUs) < 0) return;
  nextSampleUs += 1000000UL / sampleHz;
  if ((int32_t)(now - nextSampleUs) > 100000) nextSampleUs = now;  // fell behind

  float v = readVolts();
  float i = readAmps();
  Serial.print("PM,");
  Serial.print(now);
  Serial.print(',');
  Serial.print(v, 5);
  Serial.print(',');
  Serial.println(i, 6);
}
