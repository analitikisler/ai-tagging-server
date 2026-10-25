// Claims the admin page's test request and runs its body, one event as JSON,
// through the tags. Only the test container has this client, so visitor
// traffic never reaches it.
const claimRequest = require('claimRequest');
const getRequestBody = require('getRequestBody');
const JSON = require('JSON');
const returnResponse = require('returnResponse');
const runContainer = require('runContainer');
const setResponseStatus = require('setResponseStatus');

claimRequest();
runContainer(JSON.parse(getRequestBody()), () => {
  setResponseStatus(204);
  returnResponse();
});
