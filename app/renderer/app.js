'use strict';

// Boot: load config, wire the UI, then start signaling.
(async function boot() {
  App.UI.wire();
  await App.UI.reloadConfig();

  App.UI.setState('OFFLINE');
  App.UI.setStatus('red', 'server: disconnected');

  if (App.Host) App.Host.wire();
  if (App.Viewer) App.Viewer.wire();
  if (App.Caps) App.Caps.init(); // detect codec/hardware capabilities
  if (App.Signaling) App.Signaling.start();
})();
