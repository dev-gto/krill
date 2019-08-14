use std::ops::Deref;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use bytes::Bytes;
use chrono::Duration;

use rpki::uri;

use krill_commons::api;
use krill_commons::api::admin::{
    AddChildRequest, AddParentRequest, ChildAuthRequest, Handle, ParentCaContact, Token,
    UpdateChildRequest,
};
use krill_commons::api::ca::{
    CertAuthList, CertAuthSummary, ChildCaInfo, IssuedCert, RcvdCert, RepoInfo,
};
use krill_commons::api::{
    Entitlements, IssuanceRequest, IssuanceResponse, RevocationRequest, RevocationResponse,
    DFLT_CLASS,
};
use krill_commons::eventsourcing::{Aggregate, AggregateStore, DiskAggregateStore};
use krill_commons::remote::builder::SignedMessageBuilder;
use krill_commons::remote::sigmsg::SignedMessage;
use krill_commons::remote::{rfc6492, rfc8183};
use krill_commons::util::httpclient;
use krill_commons::util::softsigner::KeyId;

use crate::ca::{
    self, CertAuth, ChildHandle, CmdDet, IniDet, ParentHandle, ResourceClassName, ServerError,
    ServerResult, Signer,
};
use crate::mq::EventQueueListener;

const CA_NS: &str = "cas";

//------------ CaServer ------------------------------------------------------

#[derive(Clone)]
pub struct CaServer<S: Signer> {
    signer: Arc<RwLock<S>>,
    ca_store: Arc<DiskAggregateStore<CertAuth<S>>>,
}

impl<S: Signer> CaServer<S> {
    /// Builds a new CaServer. Will return an error if the TA store cannot be
    /// initialised.
    pub fn build(
        work_dir: &PathBuf,
        events_queue: Arc<EventQueueListener>,
        signer: S,
    ) -> ServerResult<Self, S> {
        let mut ca_store = DiskAggregateStore::<CertAuth<S>>::new(work_dir, CA_NS)?;
        ca_store.add_listener(events_queue);

        Ok(CaServer {
            signer: Arc::new(RwLock::new(signer)),
            ca_store: Arc::new(ca_store),
        })
    }

    /// Gets the TrustAnchor, if present. Returns an error if the TA is uninitialized.
    pub fn get_trust_anchor(&self) -> ServerResult<Arc<CertAuth<S>>, S> {
        self.ca_store
            .get_latest(&ca::ta_handle())
            .map_err(|_| ServerError::TrustAnchorNotInitialisedError)
    }

    /// Initialises an embedded trust anchor with all resources.
    pub fn init_ta(
        &self,
        info: RepoInfo,
        ta_aia: uri::Rsync,
        ta_uris: Vec<uri::Https>,
    ) -> ServerResult<(), S> {
        let handle = ca::ta_handle();
        if self.ca_store.has(&handle) {
            Err(ServerError::TrustAnchorInitialisedError)
        } else {
            let init = IniDet::init_ta(&handle, info, ta_aia, ta_uris, self.signer.clone())?;

            self.ca_store.add(init)?;

            Ok(())
        }
    }

    /// Republish the embedded TA and CAs if needed, i.e. if they are close
    /// to their next update time.
    pub fn republish_all(&self) -> ServerResult<(), S> {
        for ca in self.cas().cas() {
            if let Err(e) = self.republish(ca.name()) {
                error!("ServerError publishing: {}, ServerError: {}", ca.name(), e)
            }
        }
        Ok(())
    }

    /// Republish a CA, this is a no-op when there is nothing to publish.
    pub fn republish(&self, handle: &Handle) -> ServerResult<(), S> {
        debug!("Republish CA: {}", handle);
        let ca = self.ca_store.get_latest(handle)?;

        let cmd = CmdDet::publish(handle, self.signer.clone());

        let events = ca.process_command(cmd)?;
        if !events.is_empty() {
            self.ca_store.update(handle, ca, events)?;
        }

        Ok(())
    }

    /// Adds a child under the embedded TA
    pub fn ta_add_child(
        &self,
        req: AddChildRequest,
        service_uri: &uri::Https,
    ) -> ServerResult<ParentCaContact, S> {
        let (handle, resources, auth) = req.unwrap();

        debug!("Adding child {} to TA", &handle);

        let ta = self.get_trust_anchor()?;
        let ta_handle = ca::ta_handle();

        let id_cert = match &auth {
            ChildAuthRequest::Embedded => None,
            ChildAuthRequest::Rfc8183(req) => Some(req.id_cert().clone()),
        };

        let add_child = CmdDet::child_add(&ta_handle, handle.clone(), id_cert, resources);

        let events = ta.process_command(add_child)?;
        let ta = self.ca_store.update(&ta_handle, ta, events)?;

        match auth {
            ChildAuthRequest::Embedded => Ok(ParentCaContact::Embedded),
            ChildAuthRequest::Rfc8183(req) => {
                let service_uri = format!("{}rfc6492/{}", service_uri.to_string(), ta.handle());
                let service_uri = uri::Https::from_string(service_uri).unwrap();
                let service_uri = rfc8183::ServiceUri::Https(service_uri);

                let response = rfc8183::ParentResponse::new(
                    req.tag().cloned(),
                    ta.id_cert().clone(),
                    ta.handle().clone(),
                    handle,
                    service_uri,
                );
                Ok(ParentCaContact::for_rfc6492(response))
            }
        }
    }

    /// Show details for a child under the TA. Returns Ok(None) if the TA is present,
    /// but the child is not known.
    pub fn ta_show_child(&self, child: &ChildHandle) -> ServerResult<Option<ChildCaInfo>, S> {
        debug!("Finding details for {} under TA", child);

        let ta = self.get_trust_anchor()?;
        let child_opt = match ta.get_child(child) {
            Err(_) => None,
            Ok(child_details) => Some(child_details.clone().into()),
        };

        Ok(child_opt)
    }

    pub fn ta_update_child(
        &self,
        child: ChildHandle,
        req: UpdateChildRequest,
    ) -> ServerResult<(), S> {
        debug!("Updating details for {} under TA", child);
        let mut ta = self.get_trust_anchor()?;
        let ta_handle = ca::ta_handle();

        let force = req.is_force();

        let events = ta.process_command(CmdDet::child_update(&ta_handle, child.clone(), req))?;
        if !events.is_empty() {
            ta = self.ca_store.update(&ta_handle, ta, events)?;

            if force {
                let events = ta.process_command(CmdDet::child_shrink(
                    &ta_handle,
                    child,
                    Duration::seconds(0),
                    self.signer.clone(),
                ))?;
                if !events.is_empty() {
                    self.ca_store.update(&ta_handle, ta, events)?;
                }
            }
        }

        Ok(())
    }

    /// Generates a random token for embedded CAs
    pub fn random_token(&self) -> Token {
        Token::random(self.signer.read().unwrap().deref())
    }
}

/// # CA support
///
impl<S: Signer> CaServer<S> {
    pub fn get_ca(&self, handle: &Handle) -> ServerResult<Arc<CertAuth<S>>, S> {
        self.ca_store
            .get_latest(handle)
            .map_err(|_| ServerError::UnknownCa(handle.to_string()))
    }

    /// Verifies an RFC6492 message and returns the child handle, token,
    /// and content of the request, so that the simple 'list' and 'issue'
    /// functions can be called.
    pub fn rfc6492(&self, ca_handle: &Handle, msg: SignedMessage) -> ServerResult<Bytes, S> {
        debug!("RFC6492 Request: will check");
        let ca = self.ca_store.get_latest(ca_handle)?;
        let content = ca.verify_rfc6492(msg)?;
        debug!("RFC6492 Request: verified");

        let (sender, recipient, content) = content.unwrap();
        let child = Handle::from(sender.as_str());

        match content {
            rfc6492::Content::Qry(rfc6492::Qry::Revoke(req)) => {
                let res = self.revoke(ca_handle, child, req)?;
                let msg = rfc6492::Message::revoke_response(sender, recipient, res);
                self.wrap_rfc6492_response(ca_handle, msg)
            }
            rfc6492::Content::Qry(rfc6492::Qry::List) => {
                let entitlements = self.list(ca_handle, &child)?;
                let msg = rfc6492::Message::list_response(sender, recipient, entitlements);
                self.wrap_rfc6492_response(ca_handle, msg)
            }
            rfc6492::Content::Qry(rfc6492::Qry::Issue(req)) => {
                let res = self.issue(ca_handle, &child, req)?;
                let msg = rfc6492::Message::issue_response(sender, recipient, res);
                self.wrap_rfc6492_response(ca_handle, msg)
            }
            _ => Err(ServerError::custom("Unsupported RFC6492 message")),
        }
    }

    fn wrap_rfc6492_response(
        &self,
        handle: &Handle,
        msg: rfc6492::Message,
    ) -> ServerResult<Bytes, S> {
        debug!("RFC6492 Response wrapping for {}", handle);
        let ca = self.ca_store.get_latest(handle)?;
        let res = ca
            .sign_rfc6492_response(msg, self.signer.read().unwrap().deref())
            .map_err(ServerError::<S>::CertAuth);
        debug!("RFC6492 Response wrapped for {}", handle);
        res
    }

    /// List the entitlements for a child: 3.3.2 of RFC6492
    pub fn list(&self, parent: &Handle, child: &Handle) -> ServerResult<Entitlements, S> {
        if parent != &ca::ta_handle() {
            unimplemented!("https://github.com/NLnetLabs/krill/issues/25");
        } else {
            let ta = self.get_trust_anchor()?;
            Ok(ta.list(child)?)
        }
    }

    /// Issue a Certificate in response to a Certificate Issuance request
    ///
    /// See: https://tools.ietf.org/html/rfc6492#section3.4.1-2
    pub fn issue(
        &self,
        parent: &Handle,
        child: &ChildHandle,
        issue_req: IssuanceRequest,
    ) -> ServerResult<IssuanceResponse, S> {
        if parent != &ca::ta_handle() {
            unimplemented!("https://github.com/NLnetLabs/krill/issues/25");
        } else {
            let ta = self.get_trust_anchor()?;

            let class_name = issue_req.class_name();
            let pub_key = issue_req.csr().public_key();

            if class_name != DFLT_CLASS {
                unimplemented!("Issue for multiple classes from CAs, issue #25")
            }

            let cmd = CmdDet::child_certify(
                parent,
                child.clone(),
                issue_req.clone(),
                self.signer.clone(),
            );

            let events = ta.process_command(cmd)?;
            let ta = self.ca_store.update(parent, ta, events)?;

            // New entitlements will include this resource class, and
            // the newly issued certificate.
            let response = ta.issuance_response(child, &class_name, &pub_key)?;

            Ok(response)
        }
    }

    /// See: https://tools.ietf.org/html/rfc6492#section3.5.1-2
    pub fn revoke(
        &self,
        ca_handle: &Handle,
        child: ChildHandle,
        revoke_request: RevocationRequest,
    ) -> ServerResult<RevocationResponse, S> {
        let res = (&revoke_request).into(); // response provided that no errors are returned earlier

        let cmd = CmdDet::child_revoke_key(ca_handle, child, revoke_request, self.signer.clone());

        let ca = self.get_ca(ca_handle)?;
        let events = ca.process_command(cmd)?;
        self.ca_store.update(ca_handle, ca, events)?;

        Ok(res)
    }

    /// Get the current CAs
    pub fn cas(&self) -> CertAuthList {
        CertAuthList::new(
            self.ca_store
                .list()
                .into_iter()
                .map(CertAuthSummary::new)
                .collect(),
        )
    }

    /// Initialises an embedded CA, without any parents (for now).
    pub fn init_ca(
        &self,
        handle: &Handle,
        token: Token,
        repo_info: RepoInfo,
    ) -> ServerResult<(), S> {
        if self.ca_store.has(handle) {
            Err(ServerError::DuplicateCa(handle.to_string()))
        } else {
            let init = IniDet::init(handle, token, repo_info, self.signer.clone())?;
            self.ca_store.add(init)?;
            Ok(())
        }
    }

    /// Adds a parent to a CA
    pub fn ca_add_parent(&self, handle: Handle, parent: AddParentRequest) -> ServerResult<(), S> {
        let ca = self.get_ca(&handle)?;
        let (parent_handle, parent_contact) = parent.unwrap();

        let add = CmdDet::add_parent(&handle, parent_handle, parent_contact);
        let events = ca.process_command(add)?;

        self.ca_store.update(&handle, ca, events)?;

        Ok(())
    }

    /// Perform a key roll for all active keys in a CA older than the specified duration.
    pub fn ca_keyroll_init(&self, handle: Handle, max_age: Duration) -> ServerResult<(), S> {
        let ca = self.get_ca(&handle)?;

        let init_key_roll = CmdDet::key_roll_init(&handle, max_age, self.signer.clone());
        let events = ca.process_command(init_key_roll)?;

        if !events.is_empty() {
            self.ca_store.update(&handle, ca, events)?;
        }

        Ok(())
    }

    /// Activate a new key, as part of the key roll process (RFC6489). Only new keys that
    /// have an age equal to or greater than the staging period are promoted. The RFC mandates
    /// a staging period of 24 hours, but we may use a shorter period for testing and/or emergency
    /// manual key rolls.
    pub fn ca_keyroll_activate(&self, handle: Handle, staging: Duration) -> ServerResult<(), S> {
        let ca = self.get_ca(&handle)?;

        let activate_cmd = CmdDet::key_roll_activate(&handle, staging, self.signer.clone());
        let events = ca.process_command(activate_cmd)?;
        if !events.is_empty() {
            self.ca_store.update(&handle, ca, events)?;
        }

        Ok(())
    }

    /// Try to get updates for all embedded CAs, will skip the TA and/or CAs that
    /// have no parents. Will try to process all and log possible errors, i.e. do
    /// not bail out because of issues with one CA.
    pub fn get_updates_for_all_cas(&self) -> ServerResult<(), S> {
        for handle in self.ca_store.list() {
            if let Ok(ca) = self.get_ca(&handle) {
                if let Ok(parents) = ca.parents() {
                    for (parent, _info) in parents.into_iter() {
                        if let Err(e) = self.get_updates_from_parent(&handle, &parent) {
                            error!(
                                "Failed to refresh CA certificates for {}, error: {}",
                                &handle, e
                            );
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Try to update a specific CA
    pub fn get_updates_from_parent(
        &self,
        handle: &Handle,
        parent: &ParentHandle,
    ) -> ServerResult<(), S> {
        let entitlements = self.get_entitlements_from_parent(handle, parent)?;

        if !self.update_if_needed(handle, parent.clone(), entitlements)? {
            return Ok(()); // Nothing to do
        }

        self.send_requests(handle, parent)
    }

    pub fn send_requests(&self, handle: &Handle, parent: &ParentHandle) -> ServerResult<(), S> {
        self.send_revoke_requests_handle_responses(handle, parent)?;
        self.send_cert_requests_handle_responses(handle, parent)
    }

    fn send_revoke_requests_handle_responses(
        &self,
        handle: &Handle,
        parent: &ParentHandle,
    ) -> ServerResult<(), S> {
        let mut child = self.ca_store.get_latest(handle)?;
        let requests = child.revoke_requests(parent);

        let revoke_responses = self.send_revoke_requests(handle, parent, requests)?;

        for response in revoke_responses.into_iter() {
            let cmd = CmdDet::key_roll_finish(handle, parent.clone(), response);
            let events = child.process_command(cmd)?;
            child = self.ca_store.update(handle, child, events)?;
        }

        Ok(())
    }

    pub fn send_revoke_requests(
        &self,
        handle: &Handle,
        parent: &ParentHandle,
        requests: Vec<&RevocationRequest>,
    ) -> ServerResult<Vec<RevocationResponse>, S> {
        let child = self.ca_store.get_latest(handle)?;
        match child.parent(parent)?.contact() {
            ParentCaContact::Embedded => {
                self.send_revoke_requests_embedded(requests, handle, parent)
            }
            ParentCaContact::Rfc6492(parent_res) => {
                self.send_revoke_requests_rfc6492(requests, child.id_key(), parent_res)
            }
        }
    }

    fn send_revoke_requests_embedded(
        &self,
        revoke_requests: Vec<&RevocationRequest>,
        handle: &Handle,
        parent_h: &ParentHandle,
    ) -> ServerResult<Vec<RevocationResponse>, S> {
        let mut parent = self.ca_store.get_latest(parent_h)?;
        let mut revocation_responses = vec![];

        for req in revoke_requests.into_iter() {
            let cmd = CmdDet::child_revoke_key(
                parent_h,
                handle.clone(),
                req.clone(),
                self.signer.clone(),
            );

            let events = parent.process_command(cmd)?;
            parent = self.ca_store.update(parent_h, parent, events)?;

            revocation_responses.push(req.into());
        }

        Ok(revocation_responses)
    }

    fn send_revoke_requests_rfc6492(
        &self,
        revoke_requests: Vec<&RevocationRequest>,
        signing_key: &KeyId,
        parent_res: &rfc8183::ParentResponse,
    ) -> ServerResult<Vec<RevocationResponse>, S> {
        let mut res = vec![];

        for req in revoke_requests.into_iter() {
            let sender = parent_res.child_handle().to_string();
            let recipient = parent_res.parent_handle().to_string();
            let revoke = rfc6492::Message::revoke(sender, recipient, req.clone());

            match self.send_rfc6492_and_validate_response(
                signing_key,
                parent_res,
                revoke.into_bytes(),
            ) {
                Err(e) => error!("Could not send/validate revoke: {}", e),
                Ok(response) => match response {
                    rfc6492::Res::Revoke(revoke_response) => res.push(revoke_response),
                    rfc6492::Res::NotPerformed(e) => error!("We got an error response: {}", e),
                    rfc6492::Res::List(_) => error!("List response to revoke request??"),
                    rfc6492::Res::Issue(_) => error!("Issue response to revoke request??"),
                },
            }
        }

        Ok(res)
    }

    fn send_cert_requests_handle_responses(
        &self,
        handle: &Handle,
        parent: &ParentHandle,
    ) -> ServerResult<(), S> {
        let mut child = self.ca_store.get_latest(handle)?;
        let cert_requests = child.cert_requests(parent);

        let issued_certs = match child.parent(parent)?.contact() {
            ParentCaContact::Embedded => {
                self.send_cert_requests_embedded(cert_requests, handle, parent)
            }
            ParentCaContact::Rfc6492(parent_res) => {
                self.send_cert_requests_rfc6492(cert_requests, child.id_key(), &parent_res)
            }
        }?;

        for (class_name, issued) in issued_certs.into_iter() {
            let received = RcvdCert::from(issued);

            let upd_rcvd_cmd = CmdDet::upd_received_cert(
                handle,
                parent.clone(),
                class_name,
                received,
                self.signer.clone(),
            );

            let evts = child.process_command(upd_rcvd_cmd)?;
            child = self.ca_store.update(handle, child, evts)?;
        }

        Ok(())
    }

    fn send_cert_requests_embedded(
        &self,
        requests: Vec<IssuanceRequest>,
        handle: &Handle,
        parent_h: &ParentHandle,
    ) -> ServerResult<Vec<(ResourceClassName, IssuedCert)>, S> {
        let mut parent = self.ca_store.get_latest(parent_h)?;

        let mut issued_certs: Vec<(String, IssuedCert)> = vec![];

        for req in requests.into_iter() {
            let class_name = req.class_name().to_string();
            let pub_key = req.csr().public_key().clone();

            let cmd = CmdDet::child_certify(parent_h, handle.clone(), req, self.signer.clone());

            let events = parent.process_command(cmd)?;
            parent = self.ca_store.update(parent_h, parent, events)?;

            let response = parent.issuance_response(handle, &class_name, &pub_key)?;

            let (_, _, _, issued) = response.unwrap();

            issued_certs.push((class_name, issued));
        }
        Ok(issued_certs)
    }

    fn send_cert_requests_rfc6492(
        &self,
        requests: Vec<IssuanceRequest>,
        signing_key: &KeyId,
        parent_res: &rfc8183::ParentResponse,
    ) -> ServerResult<Vec<(ResourceClassName, IssuedCert)>, S> {
        let mut res = vec![];

        for req in requests.into_iter() {
            let sender = parent_res.child_handle().to_string();
            let recipient = parent_res.parent_handle().to_string();
            let issue = rfc6492::Message::issue(sender, recipient, req);

            match self.send_rfc6492_and_validate_response(
                signing_key,
                parent_res,
                issue.into_bytes(),
            ) {
                Err(e) => error!("Could not send/validate csr: {}", e),
                Ok(response) => match response {
                    rfc6492::Res::NotPerformed(e) => error!("We got an error response: {}", e),
                    rfc6492::Res::Issue(issue_response) => {
                        let (class_name, _, _, issued) = issue_response.unwrap();
                        res.push((class_name, issued));
                    }
                    rfc6492::Res::List(_) => error!("List reply to issue request??"),
                    rfc6492::Res::Revoke(_) => error!("Revoke reply to issue request??"),
                },
            }
        }

        Ok(res)
    }

    /// Updates the CA if entitlements are different from what the CA
    /// currently has under this parent. Returns [`Ok(true)`] in case
    /// there were any updates. In that case the CA will have been updated
    /// with open certificate requests which can be retrieved.
    fn update_if_needed(
        &self,
        handle: &Handle,
        parent: ParentHandle,
        entitlements: Entitlements,
    ) -> ServerResult<bool, S> {
        let child = self.ca_store.get_latest(handle)?;

        let update_entitlements_command =
            CmdDet::upd_entitlements(handle, parent, entitlements, self.signer.clone());

        let events = child.process_command(update_entitlements_command)?;
        if !events.is_empty() {
            self.ca_store.update(handle, child, events)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn get_entitlements_from_parent(
        &self,
        handle: &Handle,
        parent: &ParentHandle,
    ) -> ServerResult<api::Entitlements, S> {
        match self.get_ca(&handle)?.parent(parent)?.contact() {
            ParentCaContact::Embedded => self.get_entitlements_embedded(handle, parent),
            ParentCaContact::Rfc6492(res) => self.get_entitlements_rfc6492(handle, res),
        }
    }

    fn get_entitlements_embedded(
        &self,
        handle: &Handle,
        parent: &ParentHandle,
    ) -> ServerResult<api::Entitlements, S> {
        let parent = self.ca_store.get_latest(parent)?;
        parent.list(handle).map_err(ServerError::CertAuth)
    }

    fn get_entitlements_rfc6492(
        &self,
        handle: &Handle,
        parent_res: &rfc8183::ParentResponse,
    ) -> ServerResult<api::Entitlements, S> {
        let child = self.ca_store.get_latest(handle)?;

        // create a list request
        let sender = parent_res.child_handle().to_string();
        let recipient = parent_res.parent_handle().to_string();
        let list = rfc6492::Message::list(sender, recipient);

        let response =
            self.send_rfc6492_and_validate_response(child.id_key(), parent_res, list.into_bytes())?;

        match response {
            rfc6492::Res::NotPerformed(np) => {
                Err(ServerError::Custom(format!("Not performed: {}", np)))
            }
            rfc6492::Res::List(ent) => Ok(ent),
            _ => Err(ServerError::custom("Got unexpected response to list query")),
        }
    }

    fn send_rfc6492_and_validate_response(
        &self,
        signing_key: &KeyId,
        parent_res: &rfc8183::ParentResponse,
        msg: Bytes,
    ) -> ServerResult<rfc6492::Res, S> {
        // wrap it up and sign it
        let signed =
            { SignedMessageBuilder::create(signing_key, self.signer.read().unwrap().deref(), msg) }
                .map_err(ServerError::custom)?;

        // send to the server
        let uri = parent_res.service_uri().to_string();
        debug!(
            "Sending to parent: {}\n{}",
            &uri,
            base64::encode(&signed.as_bytes())
        );

        let res = httpclient::post_binary(&uri, &signed.as_bytes(), rfc6492::CONTENT_TYPE)
            .map_err(ServerError::HttpClientError)?;

        // unpack and validate response
        let msg = match SignedMessage::decode(res.as_ref(), false).map_err(ServerError::custom) {
            Ok(msg) => msg,
            Err(e) => {
                error!("Could not parse response: {}", base64::encode(res.as_ref()));
                return Err(e);
            }
        };

        if let Err(e) = msg.validate(parent_res.id_cert()) {
            error!(
                "Could not validate response: {}",
                base64::encode(res.as_ref())
            );
            return Err(ServerError::custom(e));
        }

        rfc6492::Message::from_signed_message(&msg)
            .map_err(ServerError::custom)?
            .into_reply()
            .map_err(ServerError::custom)
    }
}

//------------ Tests ---------------------------------------------------------

#[cfg(test)]
mod tests {

    use super::*;

    use std::path::PathBuf;
    use std::sync::{Arc, RwLock};

    use ca::EvtDet;
    use krill_commons::api::admin::{Handle, ParentCaContact, Token};
    use krill_commons::api::ca::{RcvdCert, RepoInfo, ResourceSet};
    use krill_commons::api::{IssuanceRequest, DFLT_CLASS};
    use krill_commons::eventsourcing::{Aggregate, AggregateStore, DiskAggregateStore};
    use krill_commons::util::softsigner::OpenSslSigner;
    use krill_commons::util::test;
    use krill_commons::util::test::{https, rsync, sub_dir, test_under_tmp};

    fn signer(temp_dir: &PathBuf) -> OpenSslSigner {
        let signer_dir = sub_dir(temp_dir);
        OpenSslSigner::build(&signer_dir).unwrap()
    }

    #[test]
    fn add_ta() {
        test::test_under_tmp(|d| {
            let signer = OpenSslSigner::build(&d).unwrap();

            let event_queue = Arc::new(EventQueueListener::in_mem());

            let server = CaServer::<OpenSslSigner>::build(&d, event_queue, signer).unwrap();

            let repo_info = {
                let base_uri = test::rsync("rsync://localhost/repo/ta/");
                let rrdp_uri = test::https("https://localhost/repo/notifcation.xml");
                RepoInfo::new(base_uri, rrdp_uri)
            };

            let ta_uri = test::https("https://localhost/ta/ta.cer");
            let ta_aia = test::rsync("rsync://localhost/repo/ta.cer");

            assert!(server.get_trust_anchor().is_err());

            server
                .init_ta(repo_info.clone(), ta_aia, vec![ta_uri])
                .unwrap();

            assert!(server.get_trust_anchor().is_ok());
        })
    }

    #[test]
    fn init_ta() {
        test_under_tmp(|d| {
            let ca_store = DiskAggregateStore::<CertAuth<OpenSslSigner>>::new(&d, CA_NS).unwrap();

            let ta_repo_info = {
                let base_uri = rsync("rsync://localhost/repo/ta/");
                let rrdp_uri = https("https://localhost/repo/notifcation.xml");
                RepoInfo::new(base_uri, rrdp_uri)
            };

            let ta_handle = ca::ta_handle();

            let ta_uri = https("https://localhost/tal/ta.cer");
            let ta_aia = rsync("rsync://localhost/repo/ta.cer");

            let signer = signer(&d);
            let signer = Arc::new(RwLock::new(signer));

            //
            // --- Create TA and publish
            //

            let ta_ini = IniDet::init_ta(
                &ta_handle,
                ta_repo_info,
                ta_aia,
                vec![ta_uri],
                signer.clone(),
            )
            .unwrap();

            ca_store.add(ta_ini).unwrap();
            let ta = ca_store.get_latest(&ta_handle).unwrap();

            //
            // --- Create Child CA
            //
            // Expect:
            //   - Child CA initialised
            //
            let child_handle = Handle::from("child");
            let child_token = Token::from("child");
            let child_rs = ResourceSet::from_strs("", "10.0.0.0/16", "").unwrap();

            let ca_repo_info = {
                let base_uri = rsync("rsync://localhost/repo/ca/");
                let rrdp_uri = https("https://localhost/repo/notifcation.xml");
                RepoInfo::new(base_uri, rrdp_uri)
            };

            let ca_ini = IniDet::init(
                &child_handle,
                child_token.clone(),
                ca_repo_info,
                signer.clone(),
            )
            .unwrap();

            ca_store.add(ca_ini).unwrap();
            let child = ca_store.get_latest(&child_handle).unwrap();

            //
            // --- Add Child to TA
            //
            // Expect:
            //   - Child added to TA
            //

            let cmd = CmdDet::child_add(&ta_handle, child_handle.clone(), None, child_rs);

            let events = ta.process_command(cmd).unwrap();
            let ta = ca_store.update(&ta_handle, ta, events).unwrap();

            //
            // --- Add TA as parent to child CA
            //
            // Expect:
            //   - Parent added
            //

            let parent = ParentCaContact::Embedded;

            let add_parent = CmdDet::add_parent(&child_handle, ta_handle.clone(), parent);

            let events = child.process_command(add_parent).unwrap();
            let child = ca_store.update(&child_handle, child, events).unwrap();

            //
            // --- Get resource entitlements for Child and let it process
            //
            // Expect:
            //   - No change in TA (just read-only entitlements)
            //   - Resource Class (DFLT) added to child with pending key
            //   - Certificate requested by child
            //

            let entitlements = ta.list(&child_handle).unwrap();

            let upd_ent = CmdDet::upd_entitlements(
                &child_handle,
                ta_handle.clone(),
                entitlements,
                signer.clone(),
            );

            let events = child.process_command(upd_ent).unwrap();
            assert_eq!(2, events.len()); // rc and csr
            let req_evt = events[1].clone().into_details();
            let child = ca_store.update(&child_handle, child, events).unwrap();

            let (_handle, issuance_req, _key_status) = match req_evt {
                EvtDet::CertificateRequested(parent, req, status) => (parent, req, status),
                _ => panic!("Expected Csr"),
            };

            let (class_name, limit, csr) = issuance_req.unwrap();
            assert_eq!("all", &class_name);
            assert!(limit.is_empty());

            //
            // --- Send certificate request from child to TA
            //
            // Expect:
            //   - Certificate issued
            //   - Publication
            //

            let request = IssuanceRequest::new(DFLT_CLASS.to_string(), limit, csr);

            let ta_cmd =
                CmdDet::child_certify(&ta_handle, child_handle.clone(), request, signer.clone());

            let ta_events = ta.process_command(ta_cmd).unwrap();
            let issued_evt = ta_events[0].clone().into_details();
            let _ta = ca_store.update(&ta_handle, ta, ta_events).unwrap();

            let (handle, issuance_res) = match issued_evt {
                EvtDet::ChildCertificateIssued(child, issued) => (child, issued),
                _ => panic!("Expected issued certificate."),
            };
            let (class_name, _, _, issued) = issuance_res.unwrap();

            assert_eq!(child_handle, handle);
            assert_eq!(DFLT_CLASS, class_name);

            //
            // --- Return issued certificate to child CA
            //
            // Expect:
            //   - Pending key activated
            //   - Publication

            let rcvd_cert = RcvdCert::from(issued);

            let upd_rcvd = CmdDet::upd_received_cert(
                &child_handle,
                ta_handle,
                DFLT_CLASS.to_string(),
                rcvd_cert,
                signer.clone(),
            );

            let events = child.process_command(upd_rcvd).unwrap();
            let _child = ca_store.update(&child_handle, child, events).unwrap();
        })
    }
}
