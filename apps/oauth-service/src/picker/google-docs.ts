export interface HostedGoogleDocsPickerConfiguration {
  developerKey: string;
  projectNumber: string;
  accessToken: string;
  selectionUrl: string;
}

export function hostedGoogleDocsPickerPage(configuration: HostedGoogleDocsPickerConfiguration): string {
  const serialized = JSON.stringify(configuration).replaceAll("<", "\\u003c");
  return `<!doctype html><html><head><meta charset="utf-8"><title>Choose Google Docs</title><script src="https://apis.google.com/js/api.js"></script></head><body><p id="status">Loading Google Picker…</p><script>const configuration=${serialized};let submitting=false;function submit(documentIds){if(submitting)return;submitting=true;document.getElementById('status').textContent='Sending selection to Locality…';const form=document.createElement('form');form.method='POST';form.action=configuration.selectionUrl;const input=document.createElement('input');input.name='document_ids';input.value=JSON.stringify(documentIds);form.append(input);document.body.append(form);form.submit();}gapi.load('picker',()=>{const p=google.picker;const view=new p.DocsView(p.ViewId.DOCUMENTS).setIncludeFolders(false).setSelectFolderEnabled(false).setMimeTypes('application/vnd.google-apps.document');const picker=new p.PickerBuilder().setDeveloperKey(configuration.developerKey).setAppId(configuration.projectNumber).setOAuthToken(configuration.accessToken).setOrigin(window.location.origin).addView(view).enableFeature(p.Feature.MULTISELECT_ENABLED).setCallback(data=>{const action=data[p.Response.ACTION];if(action===p.Action.PICKED){const documentIds=(data[p.Response.DOCUMENTS]||[]).map(doc=>doc.id).filter(id=>typeof id==='string'&&id.length);if(!documentIds.length){document.getElementById('status').textContent='Google Picker did not return document IDs. Try again.';return;}picker.setVisible(false);submit(documentIds);}if(action===p.Action.CANCEL)submit([]);}).build();picker.setVisible(true);});</script></body></html>`;
}
