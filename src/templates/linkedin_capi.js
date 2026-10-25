// LinkedIn Conversions API tag: sends GA4 events to LinkedIn from the server.
// Settings (tag `data`): access_token, conversions ("purchase=123,
// generate_lead=456": each GA4 event with the id of the conversion rule it
// counts for in Campaign Manager). GA4 events with no conversion rule are
// skipped, and so are events whose Consent Mode state doesn't grant both
// ad_storage and ad_user_data, and visitors LinkedIn has no way to match (no
// email and no li_fat_id click id). LinkedIn deduplicates against the Insight
// Tag by `eventId`.
const API_VERSION = '202608';

if (!data.access_token || !data.conversions) {
  data.gtmOnFailure();
  return;
}

const eventName = getEventData('event_name');
const rule = eventMap(data.conversions)[eventName];
if (!rule) {
  skip('unmapped_event');
  return;
}
const blocked = adConsentBlock();
if (blocked) {
  skip(blocked);
  return;
}

// The Insight Tag keeps the ad click's ?li_fat_id in a li_fat_id cookie.
fetchExtra((x) => {
  const userIds = [];
  const email = hash(x.email);
  if (email) userIds.push({ idType: 'SHA256_EMAIL', idValue: email });
  const fatId = queryParam('li_fat_id') || getCookieValues('li_fat_id')[0];
  if (fatId) userIds.push({ idType: 'LINKEDIN_FIRST_PARTY_ADS_TRACKING_UUID', idValue: fatId });
  if (!userIds.length) {
    skip('no_user_id');
    return;
  }
  // Names go unhashed; a value that is already a hash is left out.
  const plainValue = (v) => (v && !isHash(makeString(v).toLowerCase()) ? makeString(v).trim() : undefined);
  const userInfo = clean({
    firstName: plainValue(x.first_name),
    lastName: plainValue(x.last_name),
    countryCode: x.country && makeString(x.country).length === 2 ? makeString(x.country).toUpperCase() : undefined,
  });
  const value = numberOrUndefined(getEventData('value'));
  const currency = getEventData('currency');
  const event = clean({
    conversion: 'urn:lla:llaPartnerConversion:' + rule,
    conversionHappenedAt: now,
    conversionValue:
      value === undefined || value !== value || !currency
        ? undefined
        : { currencyCode: makeString(currency).toUpperCase(), amount: makeString(value) },
    eventId: getEventData('event_id'),
    user: clean({
      userIds: userIds,
      userInfo: userInfo.firstName && userInfo.lastName ? userInfo : undefined,
    }),
  });
  sendHttpRequest(
    'https://api.linkedin.com/rest/conversionEvents',
    {
      method: 'POST',
      headers: {
        'content-type': 'application/json',
        authorization: 'Bearer ' + data.access_token,
        'linkedin-version': API_VERSION,
        'x-restli-protocol-version': '2.0.0',
      },
    },
    JSON.stringify(event)
  ).then(finish, data.gtmOnFailure);
});
