/**
 * utils.js — Formatting helpers for device attributes, units, and timestamps
 *
 * Pure functions with no side effects.
 * Loaded as a classic script; all exports are available as globals after this file runs.
 */

/* ─────────────────────────────────────────────────────────────────────────
   Attribute value formatting
   ───────────────────────────────────────────────────────────────────────── */

/**
 * Format a device attribute value for display.
 *
 * The HomeCmdr API uses a consistent attribute shape:
 *   - Measurements:  { value: number, unit: string }
 *   - Booleans:      true | false
 *   - Strings:       "on" | "off" | any string
 *   - Numbers:       raw number
 *   - null/undefined: missing / not available
 */
function formatAttributeValue(val) {
  if (val === null || val === undefined) return '—'
  if (typeof val === 'boolean') return val ? 'yes' : 'no'
  if (typeof val === 'object') {
    if ('value' in val && 'unit' in val) {
      const formatted = typeof val.value === 'number'
        ? Number(val.value).toFixed(1)
        : val.value
      return `${formatted}\u202f${formatUnit(val.unit)}`
    }
    return JSON.stringify(val)
  }
  return String(val)
}

/**
 * Expand a raw unit string into a friendlier display form.
 * Add entries here as new adapter units are introduced.
 */
function formatUnit(unit) {
  const map = {
    celsius:    '°C',
    fahrenheit: '°F',
    kelvin:     'K',
    mireds:     'mireds',
    'km/h':     'km/h',
    'm/s':      'm/s',
    mph:        'mph',
    percent:    '%',
    lux:        'lx',
    watt:       'W',
    kwh:        'kWh',
    hpa:        'hPa',
    mm:         'mm',
    degrees:    '°',
  }
  return map[unit] || unit
}

/* ─────────────────────────────────────────────────────────────────────────
   Device attribute helpers
   ───────────────────────────────────────────────────────────────────────── */

/**
 * Read the brightness value from a device (0–100).
 * Handles both raw numbers and measurement objects.
 */
function getBrightness(device) {
  const v = device.attributes && device.attributes.brightness
  if (v === null || v === undefined) return 0
  if (typeof v === 'object') return v.value != null ? v.value : 0
  return Number(v)
}

/**
 * Read the colour temperature from a device, returning kelvin.
 * Handles both { value, unit: 'mireds' } and { value, unit: 'kelvin' } shapes.
 */
function getColorTempKelvin(device) {
  const v = device.attributes && device.attributes.color_temperature
  if (!v || typeof v !== 'object') return 4000
  return v.unit === 'mireds' ? Math.round(1000000 / v.value) : (v.value || 4000)
}

/** Return true if a device has a given attribute key. */
function hasAttribute(device, key) {
  return key in (device.attributes || {})
}

/** Return true if the device's power attribute indicates it is on. */
function isPowered(device) {
  const v = device.attributes && device.attributes.power
  return v === 'on' || v === true
}

/**
 * Return the best available display name for a device.
 * Preference: friendly_name → model → raw ID
 */
function deviceDisplayName(device) {
  return (
    (device.metadata && device.metadata.vendor_specific && device.metadata.vendor_specific.friendly_name) ||
    (device.metadata && device.metadata.vendor_specific && device.metadata.vendor_specific.model) ||
    device.id
  )
}

/**
 * Return attribute entries suitable for badge display on a sensor card.
 * Skips attributes that are shown as dedicated controls elsewhere.
 */
var CONTROL_ATTRS = new Set(['power', 'brightness', 'color_temperature', 'color', 'state'])

function sensorBadgeEntries(device) {
  return Object.entries(device.attributes || {})
    .filter(function(entry) { return !CONTROL_ATTRS.has(entry[0]) })
    .slice(0, 8)
}

/**
 * Format an attribute key for display.
 * Strips adapter-scoped prefixes, replaces underscores with spaces.
 *
 * e.g. "custom.open_meteo.wind_speed" → "wind speed"
 *      "temperature_outdoor"           → "temperature outdoor"
 */
function formatAttributeKey(key) {
  return key
    .replace(/^custom\.[^.]+\./, '')
    .replace(/_/g, ' ')
}

/* ─────────────────────────────────────────────────────────────────────────
   Time / date formatting
   ───────────────────────────────────────────────────────────────────────── */

/** Format a Date or ISO string as a short time-only string (HH:MM:SS). */
function formatTime(date) {
  return new Date(date).toLocaleTimeString()
}

/* ─────────────────────────────────────────────────────────────────────────
   URL helpers
   ───────────────────────────────────────────────────────────────────────── */

/**
 * Infer the API base URL from the current page context.
 *
 * Priority:
 *   1. ?api= query parameter
 *   2. Default local dev URL (http://127.0.0.1:3001)
 *   3. Same origin as the page (for reverse-proxy setups)
 */
function inferApiBase() {
  var fromQuery = new URLSearchParams(window.location.search).get('api')
  if (fromQuery) return fromQuery.trim().replace(/\/$/, '')

  var hostname = window.location.hostname
  if (hostname === '127.0.0.1' || hostname === 'localhost') {
    return window.location.protocol + '//127.0.0.1:3001'
  }

  return window.location.protocol + '//' + window.location.host
}

/** Read a bearer token from the ?token= query parameter, or return empty string. */
function tokenFromQuery() {
  return new URLSearchParams(window.location.search).get('token') || ''
}

/**
 * Build a WebSocket URL from an HTTP base URL, path, and optional token.
 *
 * e.g. buildWsUrl('http://127.0.0.1:3001', '/events', 'my-token')
 *      → 'ws://127.0.0.1:3001/events?token=my-token'
 */
function buildWsUrl(baseUrl, path, token) {
  var url = new URL(baseUrl.replace(/\/$/, '') + path)
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:'
  if (token) url.searchParams.set('token', token)
  return url.toString()
}
