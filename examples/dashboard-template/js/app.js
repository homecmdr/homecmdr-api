/**
 * app.js — Main Alpine.js application component
 *
 * Registers the 'homeCmdrApp' Alpine component on the 'alpine:init' event.
 * This file must be loaded BEFORE the Alpine CDN script tag so the listener
 * is in place before Alpine initialises.
 *
 * Depends on (loaded before this file):
 *   utils.js      — formatting helpers, inferApiBase(), tokenFromQuery(), buildWsUrl()
 *   api.js        — createApiClient()
 *   websocket.js  — createWebSocketManager()
 *
 * Extending this dashboard
 * ───────────────────────
 * - Add new API calls in api.js and call them from loadAll()
 * - Add new card templates in index.html using x-for / x-if
 * - Add new computed helpers below (e.g. filteredDevices(), roomSummary())
 * - Respond to new WebSocket event types in handleEvent()
 * - Add new CSS component styles in css/components.css
 */

var MAX_EVENTS = 150

// Device IDs from the open_meteo adapter that ship with the project.
// These are rendered in the Weather tab, not the general Devices tab.
var WEATHER_DEVICE_IDS = [
  'open_meteo:temperature_outdoor',
  'open_meteo:wind_speed',
  'open_meteo:wind_direction',
]

document.addEventListener('alpine:init', function() {

  Alpine.data('homeCmdrApp', function() {
    return {

      // ── Auth / connection ──────────────────────────────────────────
      apiBase:      inferApiBase(),
      apiBaseInput: inferApiBase(),
      token:        localStorage.getItem('sh_token') || tokenFromQuery(),
      tokenInput:   '',

      // ── Remote state ───────────────────────────────────────────────
      rooms:   [],
      devices: [],
      scenes:  [],
      events:  [],

      // ── UI state ───────────────────────────────────────────────────
      activeTab: 'devices',
      wsStatus:  'offline',
      error:     null,
      loading:   false,

      // ── Internal handles (not reactive) ───────────────────────────
      _api: null,
      _ws:  null,

      // ──────────────────────────────────────────────────────────────
      // Lifecycle
      // ──────────────────────────────────────────────────────────────

      init: function() {
        if (this.token) {
          this._buildClients()
          this.loadAll()
          this._ws.connect()
        }
      },

      // ──────────────────────────────────────────────────────────────
      // Auth
      // ──────────────────────────────────────────────────────────────

      connect: function() {
        var self = this
        var t = this.tokenInput.trim()
        var b = (this.apiBaseInput.trim() || this.apiBase).replace(/\/$/, '')
        if (!t) return

        this.token   = t
        this.apiBase = b
        this.error   = null
        localStorage.setItem('sh_token', t)

        this._buildClients()
        this.loadAll().then(function() {
          self._ws.connect()
        })

        this.tokenInput = ''
      },

      disconnect: function() {
        this.token = ''
        localStorage.removeItem('sh_token')
        if (this._ws) this._ws.disconnect()
        this._api    = null
        this._ws     = null
        this.rooms   = []
        this.devices = []
        this.scenes  = []
        this.events  = []
        this.error   = null
        this.wsStatus = 'offline'
      },

      // ──────────────────────────────────────────────────────────────
      // Data loading
      // ──────────────────────────────────────────────────────────────

      loadAll: function() {
        var self = this
        this.loading = true
        this.error   = null

        function ignore(e) {
          if (e.code === 'AUTH_FAILED') throw e
          console.warn('Partial load failure:', e.message)
          return []
        }

        return Promise.all([
          this._api.getRooms().catch(ignore),
          this._api.getDevices().catch(ignore),
          this._api.getScenes().catch(ignore),
        ])
        .then(function(results) {
          self.rooms   = results[0] || []
          self.devices = results[1] || []
          self.scenes  = results[2] || []
        })
        .catch(function(e) {
          if (e.code === 'AUTH_FAILED') {
            self.error = e.message
            self.token = ''
            localStorage.removeItem('sh_token')
          } else {
            self.error = 'Failed to load data: ' + e.message
          }
        })
        .finally(function() {
          self.loading = false
        })
      },

      // ──────────────────────────────────────────────────────────────
      // WebSocket event handling
      // ──────────────────────────────────────────────────────────────

      handleEvent: function(payload) {
        // Add to the live events feed (newest first), cap at MAX_EVENTS
        payload._receivedAt = formatTime(new Date())
        this.events.unshift(payload)
        if (this.events.length > MAX_EVENTS) this.events.length = MAX_EVENTS

        var type = payload.type

        if (type === 'device.state_changed') {
          // Surgical in-place attribute merge so only the changed card re-renders.
          // Calling loadAll() here would replace the entire devices array and cause
          // Alpine to re-render every card, producing a visible flash on each update.
          var found = false
          for (var i = 0; i < this.devices.length; i++) {
            if (this.devices[i].id === payload.id) {
              this.devices[i].attributes = Object.assign({}, this.devices[i].attributes, payload.state)
              found = true
              break
            }
          }
          // Device not yet in our list (genuinely new) — fall back to a full reload.
          if (!found) this.loadAll()

        } else if (type === 'device.removed') {
          var removedId = payload.id
          this.devices = this.devices.filter(function(d) { return d.id !== removedId })
        }
      },

      // ──────────────────────────────────────────────────────────────
      // Computed view helpers
      // ──────────────────────────────────────────────────────────────

      /**
       * Devices grouped by room, with unassigned devices as a trailing group.
       * Weather devices are excluded here — they appear in the Weather tab.
       */
      deviceSections: function() {
        var self = this
        var weatherSet = new Set(WEATHER_DEVICE_IDS)
        var roomIdSet  = new Set(this.rooms.map(function(r) { return r.id }))

        var sections = this.rooms
          .map(function(r) {
            return {
              id:      r.id,
              name:    r.name,
              devices: self.devices.filter(function(d) {
                return d.room_id === r.id && !weatherSet.has(d.id)
              }),
            }
          })
          .filter(function(s) { return s.devices.length > 0 })

        var unassigned = this.devices.filter(function(d) {
          return (!d.room_id || !roomIdSet.has(d.room_id)) && !weatherSet.has(d.id)
        })

        if (unassigned.length > 0) {
          sections.push({ id: '__unassigned', name: 'Unassigned', devices: unassigned })
        }

        return sections
      },

      /**
       * Look up a single weather device by its canonical ID.
       * Returns the device object, or null if not yet loaded.
       */
      weatherDevice: function(id) {
        return this.devices.find(function(d) { return d.id === id }) || null
      },

      // ── Device display helpers (thin wrappers around utils.js) ─────

      displayName: function(d)    { return deviceDisplayName(d) },
      getBrightness: function(d)  { return getBrightness(d) },
      getColorTempK: function(d)  { return getColorTempKelvin(d) },
      hasAttr: function(d, k)     { return hasAttribute(d, k) },
      isPowered: function(d)      { return isPowered(d) },
      sensorBadges: function(d)   { return sensorBadgeEntries(d) },
      fmtKey: function(k)         { return formatAttributeKey(k) },
      fmtVal: function(v)         { return formatAttributeValue(v) },

      // ──────────────────────────────────────────────────────────────
      // Commands
      // ──────────────────────────────────────────────────────────────

      togglePower: function(device) {
        var action = isPowered(device) ? 'off' : 'on'
        // Optimistic update — the adapter confirms the real state via WebSocket
        var d = this.devices.find(function(x) { return x.id === device.id })
        if (d) d.attributes = Object.assign({}, d.attributes, { power: action })
        this._sendCommand(device.id, 'power', action)
      },

      setBrightness: function(device, value) {
        this._sendCommand(device.id, 'brightness', 'set', parseInt(value, 10))
      },

      setColorTemp: function(device, kelvin) {
        this._sendCommand(device.id, 'color_temperature', 'set', {
          value: parseInt(kelvin, 10),
          unit: 'kelvin',
        })
      },

      executeScene: function(id) {
        var self = this
        this._api.executeScene(id).catch(function(e) {
          if (e.code !== 'AUTH_FAILED') self.error = 'Scene failed: ' + e.message
        })
      },

      // ──────────────────────────────────────────────────────────────
      // Private helpers
      // ──────────────────────────────────────────────────────────────

      _buildClients: function() {
        var self = this
        this._api = createApiClient(this.apiBase, this.token)
        this._ws  = createWebSocketManager({
          url:            buildWsUrl(this.apiBase, '/events', this.token),
          onMessage:      function(payload) { self.handleEvent(payload) },
          onStatusChange: function(status)  { self.wsStatus = status },
        })
      },

      _sendCommand: function(deviceId, capability, action, value) {
        var self = this
        this._api.sendCommand(deviceId, capability, action, value)
          .catch(function(e) {
            if (e.code !== 'AUTH_FAILED') self.error = 'Command failed: ' + e.message
          })
      },

    } // end return
  }) // end Alpine.data

}) // end alpine:init
