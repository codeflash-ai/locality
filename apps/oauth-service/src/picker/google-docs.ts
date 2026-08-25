export interface HostedGoogleDocsPickerConfiguration {
  developerKey: string;
  projectNumber: string;
  accessToken: string;
  selectionUrl: string;
}

export function hostedGoogleDocsPickerPage(configuration: HostedGoogleDocsPickerConfiguration): string {
  const serialized = JSON.stringify(configuration).replaceAll("<", "\\u003c");
  return `<!doctype html><html><head><meta charset="utf-8"><title>Choose Google Docs</title><script src="https://apis.google.com/js/api.js"></script></head><body><p id="status">Loading Google Picker…</p><script>const configuration=${serialized};let submitting=false;function submit(documentIds){if(submitting)return;submitting=true;document.getElementById('status').textContent='Sending selection to Locality…';const form=document.createElement('form');form.method='POST';form.action=configuration.selectionUrl;const input=document.createElement('input');input.name='document_ids';input.value=JSON.stringify(documentIds);form.append(input);document.body.append(form);form.submit();}gapi.load('picker',()=>{const p=google.picker;const view=new p.DocsView(p.ViewId.DOCUMENTS).setIncludeFolders(false).setSelectFolderEnabled(false).setMimeTypes('application/vnd.google-apps.document');const picker=new p.PickerBuilder().setDeveloperKey(configuration.developerKey).setAppId(configuration.projectNumber).setOAuthToken(configuration.accessToken).addView(view).enableFeature(p.Feature.MULTISELECT_ENABLED).setCallback(data=>{const action=data.action||(p.Response&&data[p.Response.ACTION]);const documents=data.docs||(p.Response&&data[p.Response.DOCUMENTS])||[];const pickedAction=(p.Action&&p.Action.PICKED)||'picked';const cancelAction=(p.Action&&p.Action.CANCEL)||'cancel';if(action===pickedAction){const documentIds=documents.map(doc=>doc.id||doc.documentId||(p.Document&&doc[p.Document.ID])).filter(id=>typeof id==='string'&&id.length);if(!documentIds.length){document.getElementById('status').textContent='Google Picker did not return document IDs. Try again.';return;}picker.setVisible(false);submit(documentIds);}if(action===cancelAction)submit([]);}).build();picker.setVisible(true);});</script></body></html>`;
}
