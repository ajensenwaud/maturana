// Minimal axios-through-proxy repro (the notion server uses axios) to surface
// the real Node https error. Run: NOTION_TOKEN=... node scratch-axios-repro.mjs
import axios from 'axios';

const base = 'https://api.notion.com';
const headers = {
  Authorization: `Bearer ${process.env.NOTION_TOKEN}`,
  'Notion-Version': '2022-06-28',
  'Content-Type': 'application/json',
};

async function attempt(method, url, data) {
  try {
    const res = await axios({ method, url: base + url, headers, data, timeout: 15000 });
    console.log(`${method} ${url} -> OK ${res.status} ${JSON.stringify(res.data).slice(0, 70)}`);
  } catch (e) {
    console.log(
      `${method} ${url} -> ERR msg="${e.message}" code=${e.code || ''}` +
        ` causeCode=${e.cause ? e.cause.code : ''} causeMsg="${e.cause ? e.cause.message : ''}"`,
    );
  }
}

await attempt('GET', '/v1/users');
await attempt('POST', '/v1/search', { page_size: 1 });
