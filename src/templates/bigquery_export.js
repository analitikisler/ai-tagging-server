// BigQuery export tag: writes each event as one row of a BigQuery table, for
// raw-data reporting. Settings (tag `data`): project_id, dataset_id,
// table_id, events (comma-separated GA4 event names; empty exports every
// event) and send_without_consent.
// A row is written only when the hit's Consent Mode state grants
// analytics_storage. The visitor's IP address and `user_data` (email, phone,
// address) are never written. Columns the
// table doesn't have are dropped, so a table can keep just the ones it needs.
const BigQuery = require('BigQuery');
const getAllEventData = require('getAllEventData');

if (!data.project_id || !data.dataset_id || !data.table_id) {
  data.gtmOnFailure();
  return;
}

const eventName = getEventData('event_name');
const wanted = makeString(data.events || '')
  .split(',')
  .map((s) => s.trim())
  .filter((s) => s);
if (wanted.length && wanted.indexOf(eventName) === -1) {
  skip('event_not_selected');
  return;
}

const consent = consentState();
if (consent.analytics_storage === 'denied') {
  skip('consent:analytics_storage=denied');
  return;
}
if (consent.analytics_storage === undefined && data.send_without_consent !== 'true') {
  skip('consent:analytics_storage=missing');
  return;
}

// Every event field except the IP address, the raw hit (which repeats it)
// and the buyer's email, phone and address.
const all = getAllEventData();
const eventData = {};
Object.keys(all).forEach((k) => {
  if (k !== 'ip_override' && k !== 'x-sst-raw-hit' && k !== 'user_data') eventData[k] = all[k];
});
const textOf = (v) => (v === undefined || v === null || v === '' ? undefined : makeString(v));
const items = getEventData('items');
const value = numberOrUndefined(getEventData('value'));

const row = clean({
  event_timestamp: now / 1000,
  event_name: eventName,
  event_id: textOf(getEventData('event_id')),
  measurement_id: textOf(getEventData('x-ga-measurement_id')),
  client_id: textOf(getEventData('client_id')),
  user_id: textOf(getEventData('user_id')),
  session_id: textOf(getEventData('ga_session_id')),
  page_location: textOf(pageUrl),
  page_referrer: textOf(getEventData('page_referrer')),
  page_title: textOf(getEventData('page_title')),
  value: value === value ? value : undefined,
  currency: textOf(getEventData('currency')),
  transaction_id: textOf(getEventData('transaction_id')),
  items: items && typeof items === 'object' && items.length ? JSON.stringify(items) : undefined,
  user_agent: textOf(getEventData('user_agent')),
  consent: Object.keys(consent).length ? JSON.stringify(consent) : undefined,
  event_data: JSON.stringify(eventData),
});

BigQuery.insert(
  { projectId: data.project_id, datasetId: data.dataset_id, tableId: data.table_id },
  [row],
  { ignoreUnknownValues: true }
).then(data.gtmOnSuccess, (errors) => {
  const first = (errors && errors[0]) || {};
  logToConsole('BigQuery insert failed:', first.reason || 'unknown', first.message || '');
  data.gtmOnFailure();
});
