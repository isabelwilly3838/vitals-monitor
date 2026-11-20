# Real sensor: ELEGOO UNO R3 body-temperature probe

Everything here uses parts from the **ELEGOO UNO R3 Super Starter Kit**. The Arduino
measures temperature with the kit's NTC thermistor, shows it on the LCD, signals the state
with LEDs and the buzzer, and sends it over USB. On the laptop, `vitals-bridge` turns each
reading into a **FHIR R4B body-temperature Observation** and posts it to the backend as
the `gateway` device user. Readings show up live on the dashboard under **P004 · live sensor**.

```
thermistor ─▶ UNO R3 ──USB serial "T,36.84,1"──▶ vitals-bridge ──POST /fhir/Observation──▶ backend ──▶ dashboard
              │ LCD / LEDs / buzzer                 (FHIR JSON, JWT)                          (binary WebSocket)
```

P001 to P003 stay simulated. P004 only ever receives real sensor data.

## Parts (all in the kit)

| Part | Qty |
|---|---|
| ELEGOO UNO R3 + USB cable | 1 |
| Thermistor (small black bead, 2 legs) | 1 |
| 10 kΩ resistor (brown-black-orange) | 1 |
| LCD1602 module | 1 |
| 10 kΩ potentiometer (LCD contrast) | 1 |
| LEDs: red, yellow, green | 3 |
| 220 Ω resistors (red-red-brown) | 4 |
| Active buzzer (the one with the sticker on top) | 1 |
| Breadboard + jumper wires | – |

## Wiring

Use the breadboard's **+ rail = 5V** and **− rail = GND** (wire them to the UNO's 5V and GND pins).

**Thermistor (temperature sensor):**

```
5V ──[10 kΩ]──┬── A0
              │
         [thermistor]
              │
             GND
```

**LEDs and buzzer:**

| From UNO pin | To |
|---|---|
| D2 | 220 Ω → **red** LED long leg; short leg → GND |
| D6 | 220 Ω → **yellow** LED long leg; short leg → GND |
| D5 | 220 Ω → **green** LED long leg; short leg → GND |
| D3 | buzzer **+** (longer leg); other leg → GND |

**LCD1602** (pins numbered left to right, 1 = VSS):

| LCD pin | Connect to |
|---|---|
| 1 VSS | GND |
| 2 VDD | 5V |
| 3 V0 | potentiometer middle leg (outer legs: 5V and GND) |
| 4 RS | D7 |
| 5 RW | GND |
| 6 E | D8 |
| 11 D4 | D9 |
| 12 D5 | D10 |
| 13 D6 | D11 |
| 14 D7 | D12 |
| 15 A | 220 Ω → 5V (backlight) |
| 16 K | GND |

LCD pins 7 to 10 stay unconnected. If the LCD shows nothing or only boxes, turn the potentiometer.

## 1. Upload the sketch

1. Install the **Arduino IDE** (arduino.cc/en/software).
2. Open `hardware/arduino/vitals_sensor/vitals_sensor.ino`.
3. Select **Tools → Board → Arduino Uno** and **Tools → Port → the usbmodem / usbserial port**.
4. Click **Upload**. On start-up all LEDs flash and the buzzer beeps once.
5. Optional check: **Tools → Serial Monitor** at 9600 baud should print `T,24.31,0` lines.
   **Close the Serial Monitor afterwards.** Only one program can use the port at a time.

On Mac, some ELEGOO boards use a CH340 USB chip. If no port appears, install the CH340
driver linked on ELEGOO's download page.

## 2. Start the backend and the bridge

```bash
make dev          # terminal 1: backend + dashboard on http://127.0.0.1:8000
make bridge       # terminal 2: auto-detects the Arduino and starts posting
```

The `make bridge` target runs `cargo run --release -- ` with any extra arguments in `ARGS=`, for example
`make bridge ARGS="--port /dev/cu.usbmodem1101"`. Use `--list` to see the ports.

Then log in as `clinician` and open the **P004 · live sensor** tab.

## 3. Measure

Tape the thermistor into your **armpit** and keep your arm against your body. A
thermistor needs **3 to 5 minutes** to warm up to body temperature, just like a real digital
thermometer. While the reading is changing, the LCD shows `settling...`, the yellow LED blinks
and the bridge **does not send** anything. Once the value is steady for 20 s it's
marked stable and goes to the backend once per second.

| LCD / LED | Meaning |
|---|---|
| `settling...`, yellow blinking | warming up, not sent yet |
| `Normal`, green | 35.0 to 37.9 °C |
| `HIGH - fever!`, red + 3 beeps | ≥ 38.0 °C (also raises the dashboard alert) |
| `LOW / on body?`, yellow | < 35 °C, usually the sensor isn't touching skin |
| `Check sensor!` | thermistor disconnected or shorted |

**Demo a fever safely:** hold the thermistor against a cup of warm (not hot) water, about
39 to 40 °C. It crosses 38 °C, the buzzer beeps, and the dashboard alert log shows *P004: Fever*.

## Calibration

The kit's thermistor has no exact datasheet, so the sketch assumes a typical
B = 3950 / 10 kΩ part. To calibrate, put the thermistor next to a reliable
thermometer (or in the same glass of water), compare once it's stable, and put the
difference into `CAL_OFFSET_C` in the sketch. For example, if the sketch reads 36.2 and the
thermometer reads 36.8, set `CAL_OFFSET_C = 0.6`.

## Testing without the Arduino

The bridge can read the same protocol from stdin:

```bash
cd hardware/bridge
printf 'T,36.84,1\nT,38.40,1\n' | cargo run -q -- --stdin
```

## Limitations (say this in the presentation)

- **Not a medical device.** Expect about ±0.5 to 1 °C after calibration, and armpit
  temperature is lower than core temperature.
- **Only temperature is real.** Blood pressure needs a pressure cuff, which the kit
  doesn't have, so P004 shows no BP and P001 to P003 stay simulated.
- **Timestamps come from the laptop**, because the UNO has no real-time clock.
- If the backend is down, the bridge reports the reading as *NOT sent* and keeps going. It
  doesn't store readings for later.

## Security

The bridge logs in as the **device** role, which can only *write* Observations. It can't
read any patient data. Set the password with `BRIDGE_PASSWORD`. If it's not set, the bridge
uses the dev-only default, which doesn't exist in production.
