'use strict';

const { contextBridge, ipcRenderer } = require('electron');

// One thin wrapper per IPC channel — nothing else crosses the bridge.
contextBridge.exposeInMainWorld('native', {
  configGet: () => ipcRenderer.invoke('config:get'),
  configSet: (patch) => ipcRenderer.invoke('config:set', patch),
  recentsAdd: (id) => ipcRenderer.invoke('recents:add', id),
  recentsClear: () => ipcRenderer.invoke('recents:clear'),
  passwordVerify: (plain) => ipcRenderer.invoke('password:verify', plain),
  passwordVerifyProof: (nonce, proof) => ipcRenderer.invoke('password:verifyProof', nonce, proof),
  screenSize: () => ipcRenderer.invoke('screen:size'),
  screenList: () => ipcRenderer.invoke('screen:list'),
  inputInject: (msg) => ipcRenderer.invoke('input:inject', msg),
  inputReleaseAll: () => ipcRenderer.invoke('input:releaseAll'),
  inputSetDisplay: (displayId) => ipcRenderer.invoke('input:setDisplay', displayId),
  keyhookSet: (enabled) => ipcRenderer.invoke('keyhook:set', enabled),
  // Suppressed shortcut keys pushed from main; only the {code, down} payload
  // is forwarded to the callback (never the raw event).
  onPassthroughKey: (cb) =>
    ipcRenderer.on('passthrough-key', (_e, data) => {
      if (data && typeof data.code === 'string') cb(data.code, !!data.down);
    })
});
