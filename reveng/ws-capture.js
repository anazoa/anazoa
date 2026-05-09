// Run in Web Developer Tools > Console

const OrigWS = window.WebSocket;
window._wsLog = [];

class WebSocket extends OrigWS {
  constructor(...args) {
    super(...args);
    this.addEventListener('message', e => {
      window._wsLog.push({t: Date.now(), dir: 'rx', data: e.data});
    });
  }
  send(data) {
    window._wsLog.push({t: Date.now(), dir: 'tx', data});
    return super.send(data);
  }
}
window.WebSocket = WebSocket;

// After collection:

copy(JSON.stringify(window._wsLog, null, 2));
