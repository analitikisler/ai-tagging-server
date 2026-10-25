// Example custom tag: logs each purchase. Written exactly like a Google sGTM
// tag template.
const getEventData = require('getEventData');
const logToConsole = require('logToConsole');

logToConsole('purchase', getEventData('transaction_id'), getEventData('value'), getEventData('currency'));
data.gtmOnSuccess();
