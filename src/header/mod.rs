// Copyright 2020 Nym Technologies SA
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use curve25519_dalek::scalar::Scalar;

use crypto::{EphemeralSecret, PrivateKey, SharedKey};
use keys::RoutingKeys;

use crate::constants::{HEADER_INTEGRITY_MAC_SIZE, HKDF_SALT_SIZE};
use crate::crypto;
use crate::header::delays::Delay;
use crate::header::filler::Filler;
use crate::header::keys::{BlindingFactor, KeyMaterial, PayloadKey};
use crate::header::routing::nodes::ParsedRawRoutingInformation;
use crate::header::routing::{EncapsulatedRoutingInformation, ENCRYPTED_ROUTING_INFO_SIZE};
use crate::route::{Destination, DestinationAddressBytes, Node, NodeAddressBytes, SURBIdentifier};
use crate::{Error, ErrorKind, Result};

pub mod delays;
pub mod filler;
pub mod keys;
pub mod mac;
pub mod routing;

// 32 represents size of a MontgomeryPoint on Curve25519
pub const HEADER_SIZE: usize =
    32 + HKDF_SALT_SIZE + HEADER_INTEGRITY_MAC_SIZE + ENCRYPTED_ROUTING_INFO_SIZE;
pub type HkdfSalt = [u8; 32];

#[derive(Debug)]
#[cfg_attr(test, derive(Clone))]
pub struct SphinxHeader {
    pub shared_secret: SharedKey,
    pub hkdf_salt: HkdfSalt,
    pub routing_info: EncapsulatedRoutingInformation,
}

pub enum ProcessedHeader {
    ForwardHop(SphinxHeader, NodeAddressBytes, Delay, PayloadKey),
    FinalHop(DestinationAddressBytes, SURBIdentifier, PayloadKey),
}

impl SphinxHeader {
    // needs client's secret key, how should we inject this?
    // needs to deal with SURBs too at some point
    pub fn new(
        initial_secret: &EphemeralSecret,
        route: &[Node],
        delays: &[Delay],
        hkdf_salt: &[HkdfSalt],
        destination: &Destination,
    ) -> (Self, Vec<PayloadKey>) {
        assert_eq!(route.len(), hkdf_salt.len());
        assert_eq!(route.len(), delays.len());
        let key_material = keys::KeyMaterial::derive_shared_keys(route, initial_secret);
        let routing_keys = RoutingKeys::derive_routing_keys(&key_material.shared_keys, hkdf_salt);
        let filler_string = Filler::new(&routing_keys[..route.len() - 1]);
        let routing_info = routing::EncapsulatedRoutingInformation::new(
            route,
            destination,
            &delays,
            &hkdf_salt,
            &routing_keys,
            filler_string,
        );

        // encapsulate header.routing information, compute MACs
        (
            SphinxHeader {
                shared_secret: key_material.initial_shared_group_element,
                hkdf_salt: hkdf_salt[0],
                routing_info,
            },
            routing_keys
                .iter()
                .map(|routing_key| routing_key.payload_key)
                .collect(),
        )
    }

    /// Creates a new header with a set of previously precomputed keys and initial shared secret
    pub fn new_with_precomputed_keys(
        route: &[Node],
        delays: &[Delay],
        hkdf_salt: &[HkdfSalt],
        destination: &Destination,
        shared_keys: &[SharedKey],
        initial_shared_secret: &SharedKey,
    ) -> (Self, Vec<PayloadKey>) {
        assert_eq!(route.len(), hkdf_salt.len());
        assert_eq!(route.len(), shared_keys.len());
        assert_eq!(route.len(), delays.len());
        let routing_keys = RoutingKeys::derive_routing_keys(&shared_keys, hkdf_salt);
        let filler_string = Filler::new(&routing_keys[..route.len() - 1]);
        let routing_info = routing::EncapsulatedRoutingInformation::new(
            route,
            destination,
            &delays,
            &hkdf_salt,
            &routing_keys,
            filler_string,
        );
        // encapsulate header.routing information, compute MACs
        (
            SphinxHeader {
                shared_secret: *initial_shared_secret,
                hkdf_salt: hkdf_salt[0],
                routing_info,
            },
            routing_keys
                .iter()
                .map(|routing_key| routing_key.payload_key)
                .collect(),
        )
    }

    /// Processes the header using a previously derived shared key and a fresh salt.
    /// This function can be used in the situation where sender is re-using initial secret
    /// and the intermediate nodes cash the shared key derived using Diffie Hellman as a
    /// master key, and using only the HKDF and the fresh salt derive an ephemeral key
    /// to process the packet
    pub fn process_with_previously_derived_keys(
        self,
        shared_key: SharedKey,
        hkdf_salt: Option<&HkdfSalt>,
    ) -> Result<ProcessedHeader> {
        let routing_keys = keys::RoutingKeys::derive(shared_key, hkdf_salt);
        if !self.routing_info.integrity_mac.verify(
            routing_keys.header_integrity_hmac_key,
            self.routing_info.enc_routing_information.get_value_ref(),
        ) {
            return Err(Error::new(
                ErrorKind::InvalidHeader,
                "failed to verify integrity MAC",
            ));
        }

        let unwrapped_routing_information = self
            .routing_info
            .enc_routing_information
            .unwrap(routing_keys.stream_cipher_key)?;

        match unwrapped_routing_information {
            ParsedRawRoutingInformation::ForwardHop(
                next_hop_address,
                delay,
                new_hkdf_salt,
                new_encapsulated_routing_info,
            ) => {
                let blinding_factor = KeyMaterial::compute_blinding_factor(shared_key);
                // blind the shared_secret in the header
                let new_shared_secret =
                    Self::blind_the_shared_secret(self.shared_secret, blinding_factor.to_bytes());

                Ok(ProcessedHeader::ForwardHop(
                    SphinxHeader {
                        shared_secret: new_shared_secret,
                        hkdf_salt: new_hkdf_salt,
                        routing_info: new_encapsulated_routing_info,
                    },
                    next_hop_address,
                    delay,
                    routing_keys.payload_key,
                ))
            }
            ParsedRawRoutingInformation::FinalHop(destination_address, identifier) => {
                Ok(ProcessedHeader::FinalHop(
                    destination_address,
                    identifier,
                    routing_keys.payload_key,
                ))
            }
        }
    }

    /// Processes the header using a freshly derived shared key (using Diffie Hellman)
    pub fn process(self, node_secret_key: &PrivateKey) -> Result<ProcessedHeader> {
        let shared_key = node_secret_key.diffie_hellman(&self.shared_secret);
        let routing_keys = Self::compute_routing_keys(shared_key, Some(&self.hkdf_salt));

        if !self.routing_info.integrity_mac.verify(
            routing_keys.header_integrity_hmac_key,
            self.routing_info.enc_routing_information.get_value_ref(),
        ) {
            return Err(Error::new(
                ErrorKind::InvalidHeader,
                "failed to verify integrity MAC",
            ));
        }

        let unwrapped_routing_information = self
            .routing_info
            .enc_routing_information
            .unwrap(routing_keys.stream_cipher_key)?;

        match unwrapped_routing_information {
            ParsedRawRoutingInformation::ForwardHop(
                next_hop_address,
                delay,
                new_hkdf_salt,
                new_encapsulated_routing_info,
            ) => {
                let blinding_factor = KeyMaterial::compute_blinding_factor(shared_key);
                // blind the shared_secret in the header
                let new_shared_secret =
                    Self::blind_the_shared_secret(self.shared_secret, blinding_factor.to_bytes());

                Ok(ProcessedHeader::ForwardHop(
                    SphinxHeader {
                        shared_secret: new_shared_secret,
                        hkdf_salt: new_hkdf_salt,
                        routing_info: new_encapsulated_routing_info,
                    },
                    next_hop_address,
                    delay,
                    routing_keys.payload_key,
                ))
            }
            ParsedRawRoutingInformation::FinalHop(destination_address, identifier) => {
                Ok(ProcessedHeader::FinalHop(
                    destination_address,
                    identifier,
                    routing_keys.payload_key,
                ))
            }
        }
    }

    /// Using the provided shared_secret and node's secret key, derive all routing keys for this hop.
    pub fn compute_routing_keys(
        shared_key: SharedKey,
        hkdf_salt: Option<&HkdfSalt>,
    ) -> RoutingKeys {
        keys::RoutingKeys::derive(shared_key, hkdf_salt)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        self.shared_secret
            .as_bytes()
            .iter()
            .cloned()
            .chain(
                self.hkdf_salt
                    .iter()
                    .cloned()
                    .chain(self.routing_info.to_bytes()),
            )
            .collect()
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != HEADER_SIZE {
            return Err(Error::new(
                ErrorKind::InvalidHeader,
                format!(
                    "tried to recover using {} bytes, expected {}",
                    bytes.len(),
                    HEADER_SIZE
                ),
            ));
        }

        let mut i = 0;
        let mut shared_secret_bytes = [0u8; 32];
        // first 32 bytes represent the shared secret
        shared_secret_bytes.copy_from_slice(&bytes[i..32]);
        let shared_secret = SharedKey::from(shared_secret_bytes);
        i += 32;

        let mut hkdf_salt = [0u8; HKDF_SALT_SIZE];
        hkdf_salt.copy_from_slice(&bytes[i..i + HKDF_SALT_SIZE]);
        i += HKDF_SALT_SIZE;

        // the rest are for the encapsulated routing info
        let encapsulated_routing_info_bytes =
            bytes[i..i + (HEADER_INTEGRITY_MAC_SIZE + ENCRYPTED_ROUTING_INFO_SIZE)].to_vec();

        let routing_info =
            EncapsulatedRoutingInformation::from_bytes(&encapsulated_routing_info_bytes)?;

        Ok(SphinxHeader {
            shared_secret,
            hkdf_salt,
            routing_info,
        })
    }

    fn blind_the_shared_secret(
        shared_secret: SharedKey,
        blinding_factor: BlindingFactor,
    ) -> SharedKey {
        // TODO BEFORE PR: clamping, reduction, etc.
        let blinding_factor = Scalar::from_bytes_mod_order(blinding_factor);
        let blinder: EphemeralSecret = blinding_factor.into();
        // shared_secret * blinding_factor
        blinder.diffie_hellman(&shared_secret)
    }
}

#[cfg(test)]
mod create_and_process_sphinx_packet_header {

    #[cfg(test)]
    mod with_precomputed_keys {
        use std::time::Duration;

        use crate::constants::{HKDF_SALT_SIZE, NODE_ADDRESS_LENGTH};
        use crate::crypto;
        use crate::crypto::EphemeralSecret;
        use crate::crypto::SharedKey;
        use crate::header::{delays, keys, ProcessedHeader, SphinxHeader};
        use crate::route::{Node, NodeAddressBytes};
        use crate::test_utils::fixtures::{destination_fixture, hkdf_salt_fixture};

        #[test]
        #[should_panic]
        fn it_panics_if_route_len_and_hkdf_salt_len_do_not_match() {
            let (_, node1_pk) = crypto::keygen();
            let node1 = Node {
                address: NodeAddressBytes::from_bytes([5u8; NODE_ADDRESS_LENGTH]),
                pub_key: node1_pk,
            };
            let (_, node2_pk) = crypto::keygen();
            let node2 = Node {
                address: NodeAddressBytes::from_bytes([4u8; NODE_ADDRESS_LENGTH]),
                pub_key: node2_pk,
            };
            let (_, node3_pk) = crypto::keygen();
            let node3 = Node {
                address: NodeAddressBytes::from_bytes([2u8; NODE_ADDRESS_LENGTH]),
                pub_key: node3_pk,
            };
            let route = [node1, node2, node3];
            let destination = destination_fixture();
            let initial_secret = EphemeralSecret::new();
            let average_delay = 1;
            let delays = delays::generate_from_average_duration(
                route.len(),
                Duration::from_secs(average_delay),
            );

            let hkdf_salt = [hkdf_salt_fixture(), hkdf_salt_fixture()];

            let key_material = keys::KeyMaterial::derive_shared_keys(&route, &initial_secret);
            let initial_shared_secret = SharedKey::from(&initial_secret);

            let (_, _) = SphinxHeader::new_with_precomputed_keys(
                &route,
                &delays,
                &hkdf_salt,
                &destination,
                &key_material.shared_keys,
                &initial_shared_secret,
            );
        }

        #[test]
        #[should_panic]
        fn it_panics_if_route_len_and_shared_keys_len_do_not_match() {
            let (_, node1_pk) = crypto::keygen();
            let node1 = Node {
                address: NodeAddressBytes::from_bytes([5u8; NODE_ADDRESS_LENGTH]),
                pub_key: node1_pk,
            };
            let (_, node2_pk) = crypto::keygen();
            let node2 = Node {
                address: NodeAddressBytes::from_bytes([4u8; NODE_ADDRESS_LENGTH]),
                pub_key: node2_pk,
            };
            let (_, node3_pk) = crypto::keygen();
            let node3 = Node {
                address: NodeAddressBytes::from_bytes([2u8; NODE_ADDRESS_LENGTH]),
                pub_key: node3_pk,
            };
            let route = [node1, node2, node3];
            let destination = destination_fixture();
            let initial_secret = EphemeralSecret::new();
            let average_delay = 1;
            let delays = delays::generate_from_average_duration(
                route.len(),
                Duration::from_secs(average_delay),
            );

            let hkdf_salt = [
                hkdf_salt_fixture(),
                hkdf_salt_fixture(),
                hkdf_salt_fixture(),
            ];

            let key_material = keys::KeyMaterial::derive_shared_keys(&route, &initial_secret);
            let shared_keys = &key_material.shared_keys[0..2];
            let initial_shared_secret = SharedKey::from(&initial_secret);

            let (_, _) = SphinxHeader::new_with_precomputed_keys(
                &route,
                &delays,
                &hkdf_salt,
                &destination,
                shared_keys,
                &initial_shared_secret,
            );
        }

        #[test]
        fn it_returns_correct_routing_information_for_route_of_3_mixnodes() {
            let (node1_sk, node1_pk) = crypto::keygen();
            let node1 = Node {
                address: NodeAddressBytes::from_bytes([5u8; NODE_ADDRESS_LENGTH]),
                pub_key: node1_pk,
            };
            let (node2_sk, node2_pk) = crypto::keygen();
            let node2 = Node {
                address: NodeAddressBytes::from_bytes([4u8; NODE_ADDRESS_LENGTH]),
                pub_key: node2_pk,
            };
            let (node3_sk, node3_pk) = crypto::keygen();
            let node3 = Node {
                address: NodeAddressBytes::from_bytes([2u8; NODE_ADDRESS_LENGTH]),
                pub_key: node3_pk,
            };
            let route = [node1.clone(), node2.clone(), node3.clone()];
            let destination = destination_fixture();
            let initial_secret = EphemeralSecret::new();
            let average_delay = 1;
            let delays = delays::generate_from_average_duration(
                route.len(),
                Duration::from_secs(average_delay),
            );

            let hkdf_salt = [
                hkdf_salt_fixture(),
                hkdf_salt_fixture(),
                hkdf_salt_fixture(),
            ];

            let key_material = keys::KeyMaterial::derive_shared_keys(&route, &initial_secret);
            let initial_shared_secret = SharedKey::from(&initial_secret);

            let (header, _) = SphinxHeader::new_with_precomputed_keys(
                &route,
                &delays,
                &hkdf_salt,
                &destination,
                &key_material.shared_keys,
                &initial_shared_secret,
            );

            // The first mix processing
            let shared_key1 = node1_sk.diffie_hellman(&header.shared_secret);
            let derived_unwrapped1 = match header
                .process_with_previously_derived_keys(shared_key1, Some(&hkdf_salt[0]))
                .unwrap()
            {
                ProcessedHeader::ForwardHop(new_header, next_hop_address, delay, _) => {
                    assert_eq!(node2.address, next_hop_address);
                    assert_eq!(delays[0].to_nanos(), delay.to_nanos());
                    new_header
                }
                _ => unreachable!(),
            };

            // The second mix processing
            let shared_key2 = node2_sk.diffie_hellman(&derived_unwrapped1.shared_secret);
            let derived_unwrapped2 = match derived_unwrapped1
                .process_with_previously_derived_keys(shared_key2, Some(&hkdf_salt[1]))
                .unwrap()
            {
                ProcessedHeader::ForwardHop(new_header, next_hop_address, delay, _) => {
                    assert_eq!(node3.address, next_hop_address);
                    assert_eq!(delays[1].to_nanos(), delay.to_nanos());
                    new_header
                }
                _ => unreachable!(),
            };

            // The last mix processing
            let shared_key3 = node3_sk.diffie_hellman(&derived_unwrapped2.shared_secret);
            match derived_unwrapped2
                .process_with_previously_derived_keys(shared_key3, Some(&hkdf_salt[2]))
                .unwrap()
            {
                ProcessedHeader::FinalHop(destination_address, ..) => {
                    assert_eq!(destination.address, destination_address)
                }
                _ => unreachable!(),
            };
        }

        #[test]
        fn produces_same_result_for_forward_hop_as_process_with_fresh_keys() {
            let (node1_sk, node1_pk) = crypto::keygen();
            let node1 = Node {
                address: NodeAddressBytes::from_bytes([5u8; NODE_ADDRESS_LENGTH]),
                pub_key: node1_pk,
            };
            let (node2_sk, node2_pk) = crypto::keygen();
            let node2 = Node {
                address: NodeAddressBytes::from_bytes([4u8; NODE_ADDRESS_LENGTH]),
                pub_key: node2_pk,
            };
            let route = [node1, node2];
            let destination = destination_fixture();
            let initial_secret = EphemeralSecret::new();
            let average_delay = 1;
            let delays = delays::generate_from_average_duration(
                route.len(),
                Duration::from_secs(average_delay),
            );
            let hkdf_salt = [hkdf_salt_fixture(), hkdf_salt_fixture()];
            let (sphinx_header, _) =
                SphinxHeader::new(&initial_secret, &route, &delays, &hkdf_salt, &destination);

            // The first mix processing
            let normally_unwrapped1 = match sphinx_header.clone().process(&node1_sk).unwrap() {
                ProcessedHeader::ForwardHop(new_header, ..) => new_header,
                _ => unreachable!(),
            };

            let shared_key1 = node1_sk.diffie_hellman(&sphinx_header.shared_secret);
            let derived_unwrapped1 = match sphinx_header
                .process_with_previously_derived_keys(shared_key1, Some(&hkdf_salt[0]))
                .unwrap()
            {
                ProcessedHeader::ForwardHop(new_header, ..) => new_header,
                _ => unreachable!(),
            };

            assert_eq!(
                normally_unwrapped1.shared_secret,
                derived_unwrapped1.shared_secret
            );
            assert_eq!(
                normally_unwrapped1.routing_info.to_bytes(),
                derived_unwrapped1.routing_info.to_bytes()
            );

            // The final (second) mix processing
            let normally_unwrapped2 = match derived_unwrapped1.clone().process(&node2_sk).unwrap() {
                ProcessedHeader::FinalHop(destination_address, ..) => destination_address,
                _ => unreachable!(),
            };

            let shared_key2 = node2_sk.diffie_hellman(&derived_unwrapped1.shared_secret);
            let derived_unwrapped2 = match derived_unwrapped1
                .process_with_previously_derived_keys(shared_key2, Some(&hkdf_salt[1]))
                .unwrap()
            {
                ProcessedHeader::FinalHop(destination_address, ..) => destination_address,
                _ => unreachable!(),
            };

            assert_eq!(normally_unwrapped2, derived_unwrapped2,);
        }

        #[test]
        fn produces_same_result_for_final_hop_as_process_with_fresh_keys() {
            let (node1_sk, node1_pk) = crypto::keygen();
            let node1 = Node {
                address: NodeAddressBytes::from_bytes([5u8; NODE_ADDRESS_LENGTH]),
                pub_key: node1_pk,
            };
            let route = [node1];
            let destination = destination_fixture();
            let initial_secret = EphemeralSecret::new();
            let average_delay = 1;
            let delays = delays::generate_from_average_duration(
                route.len(),
                Duration::from_secs(average_delay),
            );
            let hkdf_salt = [hkdf_salt_fixture()];
            let (sphinx_header, _) =
                SphinxHeader::new(&initial_secret, &route, &delays, &hkdf_salt, &destination);

            let normally_unwrapped = match sphinx_header.clone().process(&node1_sk).unwrap() {
                ProcessedHeader::FinalHop(destination, surb_id, keys) => {
                    (destination, surb_id, keys)
                }
                _ => unreachable!(),
            };

            let shared_key = node1_sk.diffie_hellman(&sphinx_header.shared_secret);
            let derived_unwrapped = match sphinx_header
                .process_with_previously_derived_keys(shared_key, Some(&hkdf_salt[0]))
                .unwrap()
            {
                ProcessedHeader::FinalHop(destination, surb_id, keys) => {
                    (destination, surb_id, keys)
                }
                _ => unreachable!(),
            };

            assert_eq!(normally_unwrapped.0, derived_unwrapped.0);
            assert_eq!(normally_unwrapped.1, derived_unwrapped.1);
            assert_eq!(normally_unwrapped.2.to_vec(), derived_unwrapped.2.to_vec())
        }

        #[test]
        #[should_panic]
        fn processing_with_wrong_salt_first_hop_panics() {
            // This test should panic already at the first hop, since the mac doesn't match
            let (node1_sk, node1_pk) = crypto::keygen();
            let node1 = Node {
                address: NodeAddressBytes::from_bytes([5u8; NODE_ADDRESS_LENGTH]),
                pub_key: node1_pk,
            };
            let (_, node2_pk) = crypto::keygen();
            let node2 = Node {
                address: NodeAddressBytes::from_bytes([4u8; NODE_ADDRESS_LENGTH]),
                pub_key: node2_pk,
            };
            let (_, node3_pk) = crypto::keygen();
            let node3 = Node {
                address: NodeAddressBytes::from_bytes([9u8; NODE_ADDRESS_LENGTH]),
                pub_key: node3_pk,
            };
            let route = [node1, node2, node3];
            let destination = destination_fixture();
            let initial_secret = EphemeralSecret::new();
            let average_delay = 1;
            let delays = delays::generate_from_average_duration(
                route.len(),
                Duration::from_secs(average_delay),
            );
            let hkdf_salt = [
                [25u8; HKDF_SALT_SIZE],
                [123u8; HKDF_SALT_SIZE],
                [12u8; HKDF_SALT_SIZE],
            ];
            let (sphinx_header, _) =
                SphinxHeader::new(&initial_secret, &route, &delays, &hkdf_salt, &destination);

            let incorrect_hkdf_salt = [
                [36u8; HKDF_SALT_SIZE],
                [221u8; HKDF_SALT_SIZE],
                [89u8; HKDF_SALT_SIZE],
            ];

            let shared_key = node1_sk.diffie_hellman(&sphinx_header.shared_secret);
            match sphinx_header
                .process_with_previously_derived_keys(shared_key, Some(&incorrect_hkdf_salt[0]))
                .unwrap()
            {
                ProcessedHeader::ForwardHop(new_header, ..) => new_header,
                _ => unreachable!(),
            };
        }

        #[test]
        #[should_panic]
        fn processing_with_wrong_salt_middle_hop_panics() {
            // This test should panic already at the middle hop, since the mac doesn't match
            let (node1_sk, node1_pk) = crypto::keygen();
            let node1 = Node {
                address: NodeAddressBytes::from_bytes([5u8; NODE_ADDRESS_LENGTH]),
                pub_key: node1_pk,
            };
            let (node2_sk, node2_pk) = crypto::keygen();
            let node2 = Node {
                address: NodeAddressBytes::from_bytes([4u8; NODE_ADDRESS_LENGTH]),
                pub_key: node2_pk,
            };
            let (_, node3_pk) = crypto::keygen();
            let node3 = Node {
                address: NodeAddressBytes::from_bytes([9u8; NODE_ADDRESS_LENGTH]),
                pub_key: node3_pk,
            };
            let route = [node1, node2, node3];
            let destination = destination_fixture();
            let initial_secret = EphemeralSecret::new();
            let average_delay = 1;
            let delays = delays::generate_from_average_duration(
                route.len(),
                Duration::from_secs(average_delay),
            );
            let hkdf_salt = [
                [25u8; HKDF_SALT_SIZE],
                [123u8; HKDF_SALT_SIZE],
                [12u8; HKDF_SALT_SIZE],
            ];
            let (sphinx_header, _) =
                SphinxHeader::new(&initial_secret, &route, &delays, &hkdf_salt, &destination);

            let incorrect_hkdf_salt = [
                [36u8; HKDF_SALT_SIZE],
                [221u8; HKDF_SALT_SIZE],
                [89u8; HKDF_SALT_SIZE],
            ];

            let shared_key = node1_sk.diffie_hellman(&sphinx_header.shared_secret);
            // The first one processed with correct salt
            let packet_unwrapped = match sphinx_header
                .process_with_previously_derived_keys(shared_key, Some(&hkdf_salt[0]))
                .unwrap()
            {
                ProcessedHeader::ForwardHop(new_header, ..) => new_header,
                _ => unreachable!(),
            };

            let shared_key2 = node2_sk.diffie_hellman(&packet_unwrapped.shared_secret);
            // The second one processed with incorrect salt; should panic since mac doesn't match
            match packet_unwrapped
                .process_with_previously_derived_keys(shared_key2, Some(&incorrect_hkdf_salt[1]))
                .unwrap()
            {
                ProcessedHeader::ForwardHop(new_header, ..) => new_header,
                _ => unreachable!(),
            };
        }

        #[test]
        #[should_panic]
        fn processing_with_wrong_salt_final_hop_panics() {
            // This test should panic already at the final hop, since the mac doesn't match
            let (node1_sk, node1_pk) = crypto::keygen();
            let node1 = Node {
                address: NodeAddressBytes::from_bytes([5u8; NODE_ADDRESS_LENGTH]),
                pub_key: node1_pk,
            };
            let (node2_sk, node2_pk) = crypto::keygen();
            let node2 = Node {
                address: NodeAddressBytes::from_bytes([4u8; NODE_ADDRESS_LENGTH]),
                pub_key: node2_pk,
            };
            let (node3_sk, node3_pk) = crypto::keygen();
            let node3 = Node {
                address: NodeAddressBytes::from_bytes([9u8; NODE_ADDRESS_LENGTH]),
                pub_key: node3_pk,
            };
            let route = [node1, node2, node3];
            let destination = destination_fixture();
            let initial_secret = EphemeralSecret::new();
            let average_delay = 1;
            let delays = delays::generate_from_average_duration(
                route.len(),
                Duration::from_secs(average_delay),
            );
            let hkdf_salt = [
                [25u8; HKDF_SALT_SIZE],
                [123u8; HKDF_SALT_SIZE],
                [12u8; HKDF_SALT_SIZE],
            ];
            let (sphinx_header, _) =
                SphinxHeader::new(&initial_secret, &route, &delays, &hkdf_salt, &destination);

            let incorrect_hkdf_salt = [
                [36u8; HKDF_SALT_SIZE],
                [221u8; HKDF_SALT_SIZE],
                [89u8; HKDF_SALT_SIZE],
            ];

            let shared_key = node1_sk.diffie_hellman(&sphinx_header.shared_secret);
            // The first one processed with correct salt
            let packet_unwrapped = match sphinx_header
                .process_with_previously_derived_keys(shared_key, Some(&hkdf_salt[0]))
                .unwrap()
            {
                ProcessedHeader::ForwardHop(new_header, ..) => new_header,
                _ => unreachable!(),
            };

            let shared_key2 = node2_sk.diffie_hellman(&packet_unwrapped.shared_secret);
            // The second one processed with correct salt
            let packet_unwrapped2 = match packet_unwrapped
                .process_with_previously_derived_keys(shared_key2, Some(&hkdf_salt[1]))
                .unwrap()
            {
                ProcessedHeader::ForwardHop(new_header, ..) => new_header,
                _ => unreachable!(),
            };

            let shared_key3 = node3_sk.diffie_hellman(&packet_unwrapped2.shared_secret);
            // The final one processed with incorrect salt
            match packet_unwrapped2
                .process_with_previously_derived_keys(shared_key3, Some(&incorrect_hkdf_salt[2]))
                .unwrap()
            {
                ProcessedHeader::FinalHop(destination, surb_id, keys) => {
                    (destination, surb_id, keys)
                }
                _ => unreachable!(),
            };
        }
    }

    #[cfg(test)]
    mod with_fresh_keys {
        use std::time::Duration;

        use crate::constants::NODE_ADDRESS_LENGTH;
        use crate::crypto;
        use crate::crypto::EphemeralSecret;
        use crate::header::{delays, ProcessedHeader, SphinxHeader};
        use crate::route::{Node, NodeAddressBytes};
        use crate::test_utils::fixtures::{destination_fixture, hkdf_salt_fixture};

        #[test]
        fn it_returns_correct_routing_information_at_each_hop_for_route_of_3_mixnodes() {
            let (node1_sk, node1_pk) = crypto::keygen();
            let node1 = Node {
                address: NodeAddressBytes::from_bytes([5u8; NODE_ADDRESS_LENGTH]),
                pub_key: node1_pk,
            };
            let (node2_sk, node2_pk) = crypto::keygen();
            let node2 = Node {
                address: NodeAddressBytes::from_bytes([4u8; NODE_ADDRESS_LENGTH]),
                pub_key: node2_pk,
            };
            let (node3_sk, node3_pk) = crypto::keygen();
            let node3 = Node {
                address: NodeAddressBytes::from_bytes([2u8; NODE_ADDRESS_LENGTH]),
                pub_key: node3_pk,
            };
            let route = [node1, node2, node3];
            let destination = destination_fixture();
            let initial_secret = EphemeralSecret::new();
            let average_delay = 1;
            let delays = delays::generate_from_average_duration(
                route.len(),
                Duration::from_secs(average_delay),
            );

            let hkdf_salt = [
                hkdf_salt_fixture(),
                hkdf_salt_fixture(),
                hkdf_salt_fixture(),
            ];

            let (sphinx_header, _) =
                SphinxHeader::new(&initial_secret, &route, &delays, &hkdf_salt, &destination);

            let new_header = match sphinx_header.process(&node1_sk).unwrap() {
                ProcessedHeader::ForwardHop(new_header, next_hop_address, delay, _) => {
                    assert_eq!(
                        NodeAddressBytes::from_bytes([4u8; NODE_ADDRESS_LENGTH]),
                        next_hop_address
                    );
                    assert_eq!(delays[0].to_nanos(), delay.to_nanos());
                    new_header
                }
                _ => panic!(),
            };

            let new_header2 = match new_header.process(&node2_sk).unwrap() {
                ProcessedHeader::ForwardHop(new_header, next_hop_address, delay, _) => {
                    assert_eq!(
                        NodeAddressBytes::from_bytes([2u8; NODE_ADDRESS_LENGTH]),
                        next_hop_address
                    );
                    assert_eq!(delays[1].to_nanos(), delay.to_nanos());
                    new_header
                }
                _ => panic!(),
            };
            match new_header2.process(&node3_sk).unwrap() {
                ProcessedHeader::FinalHop(final_destination, _, _) => {
                    assert_eq!(destination.address, final_destination);
                }
                _ => panic!(),
            };
        }
    }
}

#[cfg(test)]
mod unwrap_routing_information {
    use crate::constants::{
        HEADER_INTEGRITY_MAC_SIZE, NODE_ADDRESS_LENGTH, NODE_META_INFO_SIZE,
        STREAM_CIPHER_OUTPUT_LENGTH,
    };
    use crate::header::routing::nodes::{EncryptedRoutingInformation, ParsedRawRoutingInformation};
    use crate::header::routing::{ENCRYPTED_ROUTING_INFO_SIZE, FORWARD_HOP};
    use crate::{crypto, utils};

    #[test]
    fn it_returns_correct_unwrapped_routing_information() {
        let mut routing_info = [9u8; ENCRYPTED_ROUTING_INFO_SIZE];
        routing_info[0] = FORWARD_HOP;
        let stream_cipher_key = [1u8; crypto::STREAM_CIPHER_KEY_SIZE];
        let pseudorandom_bytes = crypto::generate_pseudorandom_bytes(
            &stream_cipher_key,
            &crypto::STREAM_CIPHER_INIT_VECTOR,
            STREAM_CIPHER_OUTPUT_LENGTH,
        );
        let encrypted_routing_info_vec = utils::bytes::xor(
            &routing_info,
            &pseudorandom_bytes[..ENCRYPTED_ROUTING_INFO_SIZE],
        );
        let mut encrypted_routing_info_array = [0u8; ENCRYPTED_ROUTING_INFO_SIZE];
        encrypted_routing_info_array.copy_from_slice(&encrypted_routing_info_vec);

        let enc_routing_info =
            EncryptedRoutingInformation::from_bytes(encrypted_routing_info_array);

        let expected_next_hop_encrypted_routing_information = [
            routing_info[NODE_META_INFO_SIZE + HEADER_INTEGRITY_MAC_SIZE..].to_vec(),
            pseudorandom_bytes
                [NODE_META_INFO_SIZE + HEADER_INTEGRITY_MAC_SIZE + ENCRYPTED_ROUTING_INFO_SIZE..]
                .to_vec(),
        ]
        .concat();
        let next_hop_encapsulated_routing_info =
            match enc_routing_info.unwrap(stream_cipher_key).unwrap() {
                ParsedRawRoutingInformation::ForwardHop(
                    next_hop_address,
                    _delay,
                    _next_hkdf_salt,
                    next_hop_encapsulated_routing_info,
                ) => {
                    assert_eq!(
                        routing_info[1..1 + NODE_ADDRESS_LENGTH],
                        next_hop_address.to_bytes()
                    );
                    assert_eq!(
                        routing_info
                            [NODE_ADDRESS_LENGTH..NODE_ADDRESS_LENGTH + HEADER_INTEGRITY_MAC_SIZE]
                            .to_vec(),
                        next_hop_encapsulated_routing_info
                            .integrity_mac
                            .as_bytes()
                            .to_vec()
                    );
                    next_hop_encapsulated_routing_info
                }
                _ => panic!(),
            };

        let next_hop_encrypted_routing_information = next_hop_encapsulated_routing_info
            .enc_routing_information
            .get_value_ref();

        for i in 0..expected_next_hop_encrypted_routing_information.len() {
            assert_eq!(
                expected_next_hop_encrypted_routing_information[i],
                next_hop_encrypted_routing_information[i]
            );
        }
    }
}

#[cfg(test)]
mod converting_header_to_bytes {
    use crate::constants::HKDF_SALT_SIZE;
    use crate::crypto::EphemeralSecret;
    use crate::header::SphinxHeader;
    use crate::test_utils::fixtures::encapsulated_routing_information_fixture;

    use super::*;

    #[test]
    fn it_is_possible_to_convert_back_and_forth() {
        let encapsulated_routing_info = encapsulated_routing_information_fixture();
        let hkdf_salt = [7u8; HKDF_SALT_SIZE];
        let header = SphinxHeader {
            shared_secret: SharedKey::from(&EphemeralSecret::new()),
            hkdf_salt,
            routing_info: encapsulated_routing_info,
        };

        let header_bytes = header.to_bytes();
        let recovered_header = SphinxHeader::from_bytes(&header_bytes).unwrap();

        assert_eq!(
            header.shared_secret.as_bytes(),
            recovered_header.shared_secret.as_bytes()
        );
        assert_eq!(
            header.routing_info.to_bytes(),
            recovered_header.routing_info.to_bytes()
        );
    }
}
