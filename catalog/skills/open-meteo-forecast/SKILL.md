---
name: open-meteo-forecast
description: Use when a question asks for weather or a forecast for a named place. Uses the coordinates the charter gives for that place, asks Open-Meteo for the exact variables, and reports the numbers with their units and dates.
allowed-tools: open-meteo.get-v1forecast
license: Apache-2.0
---

# Forecast from coordinates, with units

1. The place must have coordinates in the charter (Bangalore 12.97, 77.59;
   London 51.51, -0.13). A place the charter does not name: say so; do not
   guess coordinates.
2. `open-meteo.get-v1forecast` with `latitude`, `longitude`, `daily` set to
   the variables asked (`precipitation_sum,temperature_2m_max,temperature_2m_min`)
   and `timezone: auto`; `forecast_days` as asked (tomorrow = 2).
3. Report each number with its unit and its date from the `daily.time`
   array: "12.0 mm of rain in London tomorrow (2026-09-09), 18.4 °C high".
4. Never round away a unit or shift a date; never answer from memory.
