'use strict';

const { contextBridge, ipcRenderer } = require('electron');

// One thin wrapper per IPC channel — nothing else crosses the bridge.
contextBridge.exposeInMainWorld('native', {
  configGet: () => ipcRenderer.invoke('config:get'),
  configSet: (patch) => ipcRenderer.invoke('config:set', patch),
  passwordVerify: (plain) => ipcRenderer.invoke('password:verify', plain),
  passwordVerifyProof: (nonce, proof) => ipcRenderer.invoke('password:verifyProof', nonce, proof),
  screenSize: () => ipcRenderer.invoke('screen:size'),
  screenList: () => ipcRenderer.invoke('screen:list'),
  inputInject: (msg) => ipcRenderer.invoke('input:inject', msg),
  inputReleaseAll: () => ipcRenderer.invoke('input:releaseAll'),
  inputSetDisplay: (displayId) => ipcRenderer.invoke('input:setDisplay', displayId)
});
