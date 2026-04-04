import ws from 'k6/ws';
import { check } from 'k6';

export const options = {
    vus: 10000,
    duration: '60s',
};

export default function () {
    const res = ws.connect('ws://localhost:8020/ws', {}, function (socket) {
        socket.on('open', () => {
            socket.setInterval(() => {
                socket.send(JSON.stringify({ text: 'hello world' }));
            }, 5000);
        });

        socket.on('message', (msg) => {
            check(msg, { 'got response': (m) => m.length > 0 });
        });

        socket.on('error', (e) => console.error(e));
    });

    check(res, { 'connected': (r) => r && r.status === 101 });
}