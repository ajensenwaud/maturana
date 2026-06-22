// Confirm keep-alive HTTPS agent is the trigger for "socket hang up" through the
// proxy (the notion server configures axios with a keepAlive httpsAgent).
import axios from 'axios';
import https from 'node:https';

const base = 'https://api.notion.com';
const headers = {
  Authorization: `Bearer ${process.env.NOTION_TOKEN}`,
  'Notion-Version': '2022-06-28',
  'Content-Type': 'application/json',
};

async function attempt(label, agent) {
  try {
    const res = await axios({
      method: 'POST',
      url: base + '/v1/search',
      headers,
      data: { page_size: 1 },
      timeout: 15000,
      httpsAgent: agent,
    });
    console.log(`${label} -> OK ${res.status}`);
  } catch (e) {
    console.log(`${label} -> ERR "${e.message}" code=${e.code || ''} causeCode=${e.cause?.code || ''}`);
  }
}

await attempt('default-agent', undefined);
await attempt('keepAlive-agent', new https.Agent({ keepAlive: true }));
