// Minimal undici-through-proxy repro to surface the REAL error cause.
// Run on a guest: NOTION_TOKEN=... node scratch-undici-repro.mjs
import { ProxyAgent, request } from 'undici';

const proxy = process.env.HTTPS_PROXY || 'http://172.30.10.9:47833';
const agent = new ProxyAgent(proxy);

async function attempt(method, path, body) {
  try {
    const res = await request('https://api.notion.com' + path, {
      method,
      dispatcher: agent,
      headers: {
        Authorization: `Bearer ${process.env.NOTION_TOKEN}`,
        'Notion-Version': '2022-06-28',
        'Content-Type': 'application/json',
      },
      body,
    });
    let data = '';
    for await (const chunk of res.body) data += chunk;
    console.log(`${method} ${path} -> OK ${res.statusCode} ${data.slice(0, 80)}`);
  } catch (e) {
    console.log(
      `${method} ${path} -> ERR ${e.message}` +
        ` | code=${e.code || ''} | cause=${e.cause ? e.cause.message : ''}` +
        ` causeCode=${e.cause ? e.cause.code : ''}`,
    );
  }
}

await attempt('GET', '/v1/users', undefined);
await attempt('POST', '/v1/search', JSON.stringify({ page_size: 1 }));
