# innestemme as Home Assistant's Assist

The `custom_components/innestemme` integration makes the engine a conversation agent in Home Assistant. An Assist
pipeline that uses it sends every request to the engine, so the Assist button in the Home Assistant app talks to
the assistant itself, with its skills and its persona, for every user of that Home Assistant. Home Assistant's
speech to text hears the request and its text to speech speaks the answer; the engine sees only the text.

It calls `POST /api/ask` ([deployment.md](deployment.md#requests-from-a-phone)), so the engine needs `web: true`
and an `ask-token`. Home Assistant keeps the token in its config entry, once for all users.

## Install

1. In Home Assistant, Settings > Marketplace (HACS), the menu, Custom repositories: add
   `https://github.com/mortennordbye/innestemme` as an Integration, then install innestemme and restart.
2. Settings > Devices & services > Add integration > innestemme. The engine URL is where its web page answers
   (`http://<engine host>:9090`, or the HTTPS name in front of it), and the token is its `ask-token`. Setup asks
   "what time is it" once to check both; nothing is switched.
3. Settings > Voice assistants > Add assistant: name it after the assistant, pick innestemme as the conversation
   agent, and a speech to text and text to speech engine. Make it the preferred assistant to have the app's Assist
   button use it.

## What it does and does not do

- Every request goes to the engine, also "turn off the kitchen lights": Home Assistant's own intents are not
  tried first.
- The answer is in the persona's style ("As you wish, sir. The kitchen lights are off."), as it is spoken in the
  room. An answer that would end quietly is "Done." in Assist, which needs words.
- "Turn off the lights" without a room means the satellite's room, as for every request that arrives as text.
- The engine's own wake word, voice and speech cache are not used: the pipeline's speech engines are.
- A wrong token or an engine that does not answer gives an error in Assist ("innestemme refused the token.",
  "innestemme is not answering.").
