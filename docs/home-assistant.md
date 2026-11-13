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
3. For the engine's own ears and voice, set `wyoming-bind` (for example `0.0.0.0:10300`) and add the Wyoming
   Protocol integration in Home Assistant with the engine's host and that port. It offers the engine's listener as
   speech to text and its voice as text to speech, both "innestemme".
4. Settings > Voice assistants > Add assistant: name it after the assistant, and pick innestemme as the
   conversation agent, speech to text and text to speech. Make it the preferred assistant to have the app's Assist
   button use it.

## What it does and does not do

- Every request goes to the engine, also "turn off the kitchen lights": Home Assistant's own intents are not
  tried first.
- The answer is in the persona's style ("As you wish, sir. The kitchen lights are off."), as it is spoken in the
  room. An answer that would end quietly is "Done." in Assist, which needs words.
- "Turn off the lights" without a room means the satellite's room, as for every request that arrives as text.
- With the Wyoming engines, the request is transcribed by the satellite's listener and the answer is spoken in its
  voice, sentence by sentence through the speech cache, so answers rendered ahead play at once. Home Assistant
  still decides when the speaker has stopped. Wyoming has no authentication: whoever reaches the port can
  transcribe and synthesize, which changes nothing in the house.
- Audio up to 30 s and text up to 1000 characters are taken; the engine's wake word is not used.
- A wrong token or an engine that does not answer gives an error in Assist ("innestemme refused the token.",
  "innestemme is not answering.").
