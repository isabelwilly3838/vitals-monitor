// vitals_sensor.ino - body-temperature sensor for the Vitals Monitor
// Board: ELEGOO UNO R3 (Arduino Uno compatible). Parts: all from the Super Starter Kit.
//
// Reads a 10k NTC thermistor, shows the value on the LCD1602, signals the state with
// LEDs + buzzer, and reports over USB serial for the bridge (hardware/bridge) to turn
// into FHIR Observations.  Wiring: see hardware/README.md.
//
// Serial protocol (9600 baud, one line per second, '\n' terminated):
//   # vitals-sensor v1 thermistor     banner at boot (ignored by the bridge)
//   T,<celsius>,<stable>               e.g. "T,36.84,1"  stable = 1 when settled
//   E,<reason>                         sensor fault, e.g. "E,open" or "E,short"

#include <LiquidCrystal.h>
#include <math.h>

// ---- pins ---------------------------------------------------------------------------
const uint8_t PIN_THERM = A0;
const uint8_t PIN_LED_RED = 2;     // fever
const uint8_t PIN_BUZZER = 3;      // active buzzer (the one with the sticker)
const uint8_t PIN_LED_GREEN = 5;   // normal
const uint8_t PIN_LED_YELLOW = 6;  // settling / low
LiquidCrystal lcd(7, 8, 9, 10, 11, 12);  // RS, E, D4, D5, D6, D7

// ---- thermistor model (Beta equation) ------------------------------------------------
// Divider: 5V -- 10k resistor -- A0 -- thermistor -- GND
const float SERIES_R = 10000.0;    // the fixed resistor (brown-black-orange)
const float NOMINAL_R = 10000.0;   // thermistor resistance at 25 C
const float NOMINAL_T = 25.0;
const float BETA = 3950.0;         // typical for 10k NTCs; calibrate if readings are off
const float CAL_OFFSET_C = 0.0;    // add after comparing with a real thermometer

// ---- clinical thresholds (same as the backend: fhir/codes.rs) ------------------------
const float FEVER_C = 38.0;
const float LOW_C = 35.0;

// ---- stability detection: like a digital thermometer, wait until the value settles ---
const uint8_t WINDOW = 20;         // seconds of history
const float STABLE_SPAN_C = 0.15;  // max - min within the window
float history[WINDOW];
uint8_t histCount = 0, histPos = 0;

float smoothed = NAN;
bool feverAlarmed = false;
unsigned long lastReport = 0;

float readCelsius(int &rawOut) {
  long sum = 0;
  for (uint8_t i = 0; i < 16; i++) {  // oversample to reduce ADC noise
    sum += analogRead(PIN_THERM);
    delay(2);
  }
  float raw = sum / 16.0;
  rawOut = (int)raw;
  float r = SERIES_R * raw / (1023.0 - raw);
  float invT = 1.0 / (NOMINAL_T + 273.15) + log(r / NOMINAL_R) / BETA;
  return 1.0 / invT - 273.15 + CAL_OFFSET_C;
}

bool isStable() {
  if (histCount < WINDOW) return false;
  float lo = history[0], hi = history[0];
  for (uint8_t i = 1; i < WINDOW; i++) {
    lo = min(lo, history[i]);
    hi = max(hi, history[i]);
  }
  return hi - lo <= STABLE_SPAN_C;
}

void setLeds(bool red, bool yellow, bool green) {
  digitalWrite(PIN_LED_RED, red);
  digitalWrite(PIN_LED_YELLOW, yellow);
  digitalWrite(PIN_LED_GREEN, green);
}

void beep(uint8_t times) {
  for (uint8_t i = 0; i < times; i++) {
    digitalWrite(PIN_BUZZER, HIGH);
    delay(120);
    digitalWrite(PIN_BUZZER, LOW);
    delay(120);
  }
}

void showLcd(const char *line1, const char *line2) {
  lcd.setCursor(0, 0);
  lcd.print(line1);
  lcd.setCursor(0, 1);
  lcd.print(line2);
}

void setup() {
  pinMode(PIN_LED_RED, OUTPUT);
  pinMode(PIN_LED_YELLOW, OUTPUT);
  pinMode(PIN_LED_GREEN, OUTPUT);
  pinMode(PIN_BUZZER, OUTPUT);
  Serial.begin(9600);
  lcd.begin(16, 2);
  showLcd("Vitals Monitor  ", "starting...     ");
  setLeds(true, true, true);
  beep(1);
  delay(800);
  setLeds(false, false, false);
  Serial.println(F("# vitals-sensor v1 thermistor"));
}

void loop() {
  if (millis() - lastReport < 1000) return;
  lastReport = millis();

  int raw;
  float c = readCelsius(raw);

  // A disconnected or shorted sensor gives an ADC value at the rails.
  if (raw > 1015 || raw < 8) {
    const char *why = raw > 1015 ? "open" : "short";
    Serial.print(F("E,"));
    Serial.println(why);
    showLcd("Check sensor!   ", raw > 1015 ? "not connected   " : "short circuit   ");
    setLeds(false, millis() / 1000 % 2, false);
    histCount = 0;
    smoothed = NAN;
    return;
  }

  smoothed = isnan(smoothed) ? c : 0.7 * smoothed + 0.3 * c;  // light smoothing
  history[histPos] = smoothed;
  histPos = (histPos + 1) % WINDOW;
  if (histCount < WINDOW) histCount++;
  bool stable = isStable();

  Serial.print(F("T,"));
  Serial.print(smoothed, 2);
  Serial.print(',');
  Serial.println(stable ? 1 : 0);

  char line1[17], line2[17], num[8];
  dtostrf(smoothed, 5, 1, num);
  snprintf(line1, sizeof line1, "Temp %s %cC   ", num, (char)223);  // 223 = degree sign

  if (!stable) {
    snprintf(line2, sizeof line2, "settling...     ");
    setLeds(false, millis() / 1000 % 2, false);  // blink yellow
  } else if (smoothed >= FEVER_C) {
    snprintf(line2, sizeof line2, "HIGH - fever!   ");
    setLeds(true, false, false);
    if (!feverAlarmed) beep(3);  // beep once per episode, not continuously
    feverAlarmed = true;
  } else if (smoothed < LOW_C) {
    snprintf(line2, sizeof line2, "LOW / on body?  ");
    setLeds(false, true, false);
  } else {
    snprintf(line2, sizeof line2, "Normal          ");
    setLeds(false, false, true);
  }
  if (stable && smoothed < FEVER_C - 0.2) feverAlarmed = false;  // hysteresis
  showLcd(line1, line2);
}
