/**
 * api.js — Authenticated HTTP client for the HomeCmdr API
 *
 * Exposes: createApiClient(baseUrl, token) → client object
 *
 * Usage:
 *   var client = createApiClient('http://127.0.0.1:3001', 'your-token')
 *   client.getDevices().then(function(devices) { ... })
 *   client.sendCommand('zigbee2mqtt:bulb_1', 'power', 'on')
 *
 * All methods return Promises that resolve with parsed JSON on success.
 * A 401/403 rejects with an Error whose .code property is 'AUTH_FAILED',
 * so callers can handle token expiry separately from other errors.
 */

function createApiClient(baseUrl, token) {
  var base = baseUrl.replace(/\/$/, '')

  function request(path, options) {
    options = options || {}
    var headers = {
      'Content-Type': 'application/json',
    }
    if (token) {
      headers['Authorization'] = 'Bearer ' + token
    }
    // Allow callers to merge extra headers
    Object.assign(headers, options.headers || {})

    return fetch(base + path, Object.assign({}, options, { headers: headers }))
      .then(function(response) {
        if (response.status === 401 || response.status === 403) {
          var err = new Error('Token rejected — please reconnect with a valid token.')
          err.code = 'AUTH_FAILED'
          throw err
        }
        if (!response.ok) {
          return response.text().catch(function() { return '' }).then(function(body) {
            throw new Error('API error ' + response.status + ': ' + (body || response.statusText))
          })
        }
        if (response.status === 204) return null
        return response.json()
      })
  }

  return {
    /**
     * Fetch all rooms.
     * GET /rooms
     */
    getRooms: function() {
      return request('/rooms')
    },

    /**
     * Fetch devices. Pass an array of IDs to filter the response.
     * GET /devices
     * GET /devices?ids=id1&ids=id2
     */
    getDevices: function(ids) {
      ids = ids || []
      var query = ids.map(function(id) { return 'ids=' + encodeURIComponent(id) }).join('&')
      return request('/devices' + (query ? '?' + query : ''))
    },

    /**
     * Fetch all loaded scenes.
     * GET /scenes
     */
    getScenes: function() {
      return request('/scenes')
    },

    /**
     * Send a capability command to a device.
     * POST /devices/{id}/command
     *
     * Examples:
     *   sendCommand('zigbee2mqtt:bulb_1', 'power', 'on')
     *   sendCommand('zigbee2mqtt:bulb_1', 'brightness', 'set', 75)
     *   sendCommand('elgato_lights:key_light', 'color_temperature', 'set', { value: 4000, unit: 'kelvin' })
     */
    sendCommand: function(deviceId, capability, action, value) {
      var body = { capability: capability, action: action }
      if (value !== undefined) body.value = value
      return request('/devices/' + encodeURIComponent(deviceId) + '/command', {
        method: 'POST',
        body: JSON.stringify(body),
      })
    },

    /**
     * Execute a scene by ID.
     * POST /scenes/{id}/execute
     */
    executeScene: function(sceneId) {
      return request('/scenes/' + encodeURIComponent(sceneId) + '/execute', {
        method: 'POST',
      })
    },
  }
}
